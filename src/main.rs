use anyhow::Result;
use die::{die, Die};
use futures::stream::StreamExt;
use rusqlite::Connection;
use rustkov::prelude::Brain;
use serde_derive::Deserialize;
use std::{
    collections::HashMap,
    convert::TryFrom,
    env::args,
    fs::File,
    io::Read,
    iter::Iterator,
    ops::Sub,
    path::Path,
    time::{Duration, Instant},
};
use tokio_xmpp::AsyncClient as Client;
use xmpp_parsers::{
    message::{Body, Message, MessageType},
    muc::{muc::History, Muc},
    presence::{Presence, Type as PresenceType},
    BareJid, Element, FullJid, Jid,
};

struct Room {
    nick: String,
    chain_indices: Vec<usize>,
    jid: FullJid,

    last_sent_jabber: Instant,
    last_sent_dad: Instant,
    last_sent_random: Instant,
}

impl Room {
    // executed for every "botname: command-here" message with the nick and whitespace trimmed from front
    fn directed_message(&mut self, orig_body: &str, brain: &mut Brain) -> Result<Option<String>> {
        let body = orig_body.to_lowercase();
        Ok(match body.as_str() {
            "jabber" => Some(XMPP_NOT_JABBER.to_string()),
            "dad" => choose(DAD_JOKES),
            "repo" | "code" => Some("https://github.com/moparisthebest/ash".to_string()),
            "words" => Some(format!("I know {} words!", brain.stats().get_total_words())),
            _ => brain.generate(orig_body)?,
        })
    }

    // executed for every other message
    fn non_directed_message(
        &mut self,
        orig_body: &str,
        brain: &mut Brain,
    ) -> Result<Option<String>> {
        let body = orig_body.to_lowercase();
        if should_send(&body, &mut self.last_sent_jabber, "jabber", 120, 0.5) {
            return Ok(Some(XMPP_NOT_JABBER.to_string()));
        }
        if should_send(&body, &mut self.last_sent_dad, "dad", 300, 0.5) {
            return Ok(choose(DAD_JOKES));
        }
        if should_send(&body, &mut self.last_sent_random, "", 300, 0.01) {
            // 50% chance dad joke vs brain
            return Ok(if chance(0.5) {
                choose(DAD_JOKES)
            } else {
                brain.generate(orig_body)?
            });
        }
        Ok(None)
    }

    fn new(nick: String, jid: FullJid, chain_indices: Vec<usize>) -> Self {
        let long_ago = Instant::now().sub(Duration::from_secs(99999));
        Self {
            nick,
            chain_indices,
            jid,
            last_sent_jabber: long_ago,
            last_sent_dad: long_ago,
            last_sent_random: long_ago,
        }
    }
}

#[derive(Deserialize)]
struct Config {
    jid: String,
    password: String,
    db: Option<String>,
    nick: Option<String>,
    rooms: Vec<RoomConfig>,
}

#[derive(Deserialize)]
struct RoomConfig {
    room: String,
    chain_indices: Option<Vec<usize>>,
    nick: Option<String>,
}

fn parse_cfg<P: AsRef<Path>>(path: P) -> Result<Config> {
    let mut f = File::open(path)?;
    let mut input = String::new();
    f.read_to_string(&mut input)?;
    Ok(toml::from_str(&input)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let first_arg = args().nth(1);

    let cfg = match first_arg.as_deref() {
        Some("-h") | Some("--help") => {
            die!("usage: ash [/path/to/config.toml]")
        }
        Some(config) => parse_cfg(config).die("provided config cannot be found/parsed"),
        None => parse_cfg(
            dirs::config_dir()
                .die("cannot find home directory")
                .join("ash.toml"),
        )
        .or_else(|_| parse_cfg("/etc/ash/ash.toml"))
        .die("valid config file not found"),
    };

    if cfg.rooms.is_empty() {
        die!("no rooms specified!");
    }

    let mut rooms: HashMap<(String, String), Room> = HashMap::with_capacity(cfg.rooms.len());

    let mut max_idx = 0;
    let jid: BareJid = cfg.jid.parse()?;
    for room in cfg.rooms {
        let nick = room
            .nick
            .as_ref()
            .or(cfg.nick.as_ref())
            .or(jid.node.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("ash")
            .to_string();
        let jid: BareJid = room.room.parse()?;
        let jid = jid.with_resource(&nick);
        let mut chain_indices = room.chain_indices.unwrap_or_else(|| vec![0]);
        // always push everything to 0
        if !chain_indices.contains(&0) {
            chain_indices.push(0);
        }
        let max = *chain_indices
            .iter()
            .max()
            .expect("must exist due to above push");
        if max > max_idx {
            max_idx = max;
        }
        rooms.insert(
            (
                jid.node
                    .as_ref()
                    .die("room jids must have local part")
                    .clone(),
                jid.domain.clone(),
            ),
            Room::new(nick, jid, chain_indices),
        );
    }

    let conn = Connection::open(cfg.db.as_deref().unwrap_or("ash.db"))?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS msg (
            id    INTEGER PRIMARY KEY,
            node  TEXT NOT NULL,
            domain  TEXT NOT NULL,
            nick  TEXT NOT NULL,
            msg  TEXT NOT NULL
        )",
        (), // empty list of parameters.
    )?;

    let mut brain = vec![Brain::new(); max_idx + 1];
    let mut stmt = conn.prepare("SELECT node, domain, msg from msg;")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let node: String = row.get(0)?;
        let domain: String = row.get(1)?;
        let msg: String = row.get(2)?;
        //println!("Found msg: {node}@{domain} - {msg}");
        if let Some(room) = rooms.get(&(node, domain)) {
            for x in &room.chain_indices {
                brain[*x].ingest(&msg);
            }
        } else {
            // for now we are going to put *everything* in idx 0
            brain[0].ingest(&msg);
        }
    }

    let mut client = Client::new(&cfg.jid, &cfg.password)?;
    client.set_reconnect(true);

    while let Some(event) = client.next().await {
        if event.is_online() {
            for room in rooms.values() {
                let join = make_join(room.jid.clone());
                client.send_stanza(join).await?;
            }
        } else if let Some(message) = event
            .into_stanza()
            .and_then(|stanza| Message::try_from(stanza).ok())
        {
            match (&message.from, message.bodies.get("")) {
                (Some(ref from), Some(ref body)) => {
                    if message.type_ != MessageType::Error {
                        match from {
                            Jid::Full(FullJid {
                                node: Some(node),
                                domain,
                                resource,
                            }) => {
                                if let Some(room) =
                                    rooms.get_mut(&(node.to_string(), domain.to_string()))
                                {
                                    let nick = &room.nick;
                                    if resource == nick {
                                        continue;
                                    }
                                    let body = &body.0;
                                    println!("from: '{from}', body: {body}");
                                    let response = if body.starts_with(nick) {
                                        let body = body.trim_start_matches(nick);
                                        let body = body.trim_start_matches([',', ':', ' ']);
                                        println!("self body: {body}");
                                        room.directed_message(
                                            body,
                                            &mut brain[room.chain_indices[0]],
                                        )?
                                    } else {
                                        room.non_directed_message(
                                            body,
                                            &mut brain[room.chain_indices[0]],
                                        )?
                                    };
                                    if let Some(response) = response {
                                        println!("reply: {}", response);
                                        // todo: reply to from or just node+domain ?
                                        let from = Jid::Bare(BareJid {
                                            node: Some(node.to_string()),
                                            domain: domain.to_string(),
                                        });
                                        client
                                            .send_stanza(make_reply(from.clone(), &response))
                                            .await?;
                                    }
                                    conn.execute("INSERT INTO msg (node, domain, nick, msg) values (?, ?, ?, ?)", [node, domain, resource, body])?;
                                    for x in &room.chain_indices {
                                        brain[*x].ingest(body);
                                    }
                                } else {
                                    println!("ignoring: from: '{from}', body: {body:?}");
                                }
                            }
                            _ => println!("ignoring: from: '{from}', body: {body:?}"),
                        }
                    }
                }
                _ => println!("ignoring: message: '{message:?}'"),
            }
        }
    }

    // Close client connection
    client.send_end().await.ok(); // ignore errors here, I guess

    Ok(())
}

fn make_join(to: FullJid) -> Element {
    Presence::new(PresenceType::None)
        .with_to(Jid::Full(to))
        .with_payloads(vec![Muc::new()
            .with_history(History::new().with_maxstanzas(0))
            .into()])
        .into()
}

// Construct a chat <message/>
fn make_reply(to: Jid, body: &str) -> Element {
    let mut message = Message::new(Some(to));
    message.type_ = MessageType::Groupchat;
    message.bodies.insert(String::new(), Body(body.to_owned()));
    message.into()
}

fn should_send(
    body: &str,
    last_sent: &mut Instant,
    pattern: &str,
    min_seconds: u64,
    pct: f64,
) -> bool {
    let now = Instant::now();
    let last_sent_seconds = (now - *last_sent).as_secs();
    if last_sent_seconds >= min_seconds
        && (pattern.is_empty() || body.contains(pattern))
        && chance(pct)
    {
        *last_sent = now;
        true
    } else {
        false
    }
}

fn chance(pct: f64) -> bool {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    pct > rng.gen_range(0f64..1f64)
}

fn choose(choices: &[&str]) -> Option<String> {
    use rand::{prelude::SliceRandom, thread_rng};
    let mut rng = thread_rng();
    choices.choose(&mut rng).map(|s| s.to_string())
}

const XMPP_NOT_JABBER: &str = "I'd just like to interject for a moment. What you're referring to as Jabber, is in fact, XMPP, or as I've recently taken to calling it, XMPP not Jabber. Jabber is not an internet protocol unto itself, but rather another proprietary product owned by Cisco. XMPP instead is a fully functioning free protocol made useful by standardization and extensibility.
";

const DAD_JOKES: &[&str] = &[
    "I'm tired of following my dreams. I'm just going to ask them where they are going and meet up with them later.",
    "Did you hear about the guy whose whole left side was cut off? He's all right now.",
    "Why didn’t the skeleton cross the road? Because he had no guts.",
    "What did one nut say as he chased another nut?  I'm a cashew!",
    "Where do fish keep their money? In the riverbank",
    "I accidentally took my cats meds last night. Don’t ask meow.",
    "Chances are if you' ve seen one shopping center, you've seen a mall.",
    "Dermatologists are always in a hurry. They spend all day making rash decisions. ",
    "I knew I shouldn't steal a mixer from work, but it was a whisk I was willing to take.",
    "I won an argument with a weather forecaster once. His logic was cloudy...",
    "How come the stadium got hot after the game? Because all of the fans left.",
    "\"Why do seagulls fly over the ocean?\" \"Because if they flew over the bay, we'd call them bagels.\"",
    "Why was it called the dark ages? Because of all the knights. ",
    "A steak pun is a rare medium well done.",
    "Why did the tomato blush? Because it saw the salad dressing.",
    "Did you hear the joke about the wandering nun? She was a roman catholic.",
    "What creature is smarter than a talking parrot? A spelling bee.",
    "I'll tell you what often gets over looked... garden fences.",
    "Why did the kid cross the playground? To get to the other slide.",
    "Why do birds fly south for the winter? Because it's too far to walk.",
    "What is a centipedes's favorite Beatle song?  I want to hold your hand, hand, hand, hand...",
    "My first time using an elevator was an uplifting experience. The second time let me down.",
    "To be Frank, I'd have to change my name.",
    "Slept like a log last night … woke up in the fireplace.",
    "What do you call a female snake. misssssssss ",
    "Why does a Moon-rock taste better than an Earth-rock? Because it's a little meteor.",
    "I thought my wife was joking when she said she'd leave me if I didn't stop signing \"I'm A Believer\"... Then I saw her face.",
    "I’m only familiar with 25 letters in the English language. I don’t know why.",
    "What do you call two barracuda fish?  A Pairacuda!",
    "What did the father tomato say to the baby tomato whilst on a family walk? Ketchup.",
    "Why is Peter Pan always flying? Because he Neverlands.",
    "What do you do on a remote island? Try and find the TV island it belongs to.",
    "Did you know that protons have mass? I didn't even know they were catholic.",
    "Dad I’m hungry’ … ‘Hi hungry I’m dad",
    "I was fired from the keyboard factory yesterday.  I wasn't putting in enough shifts.",
    "Whoever invented the knock-knock joke should get a no bell prize.",
    "Wife: Honey I’m pregnant.\r\n\r\nMe: Well…. what do we do now?\r\n\r\nWife: Well, I guess we should go to a baby doctor.\r\n\r\nMe: Hm.. I think I’d be a lot more comfortable going to an adult doctor.",
    "Have you heard the story about the magic tractor? It drove down the road and turned into a field.",
    "When will the little snake arrive? I don't know but he won't be long...",
    "Why was Pavlov's beard so soft?  Because he conditioned it.",
    "Do I enjoy making courthouse puns? Guilty",
    "Why did the kid throw the clock out the window? He wanted to see time fly!",
    "Hear about the new restaurant called Karma? There’s no menu: You get what you deserve.",
    "Why couldn't the kid see the pirate movie? Because it was rated arrr!",
    "It was so cold yesterday my computer froze. My own fault though, I left too many windows open.",
    "Man, I really love my furniture... me and my recliner go way back.",
    "What did the traffic light say to the car as it passed? \"Don't look I'm changing!\"",
    "My son is studying to be a surgeon, I just hope he makes the cut.",
    "Why did the man run around his bed? Because he was trying to catch up on his sleep!",
    "What did one wall say to the other wall? I'll meet you at the corner!",
    "Sometimes I tuck my knees into my chest and lean forward.  That’s just how I roll.",
    "I once lost a banana at court but then I appealed. ",
    "Conjunctivitis.com – now that’s a site for sore eyes.",
    "How many South Americans does it take to change a lightbulb? A Brazilian",
    "I don't trust stairs. They're always up to something.",
    "What do you call two guys hanging out by your window? Kurt & Rod.",
    "Why was the robot angry? Because someone kept pressing his buttons!",
    "Which is the fastest growing city in the world? Dublin'",
    "A police officer caught two kids playing with a firework and a car battery. He charged one and let the other one off.",
    "What do you call a snake who builds houses? A boa constructor!",
    "What is the difference between ignorance and apathy?\r\n\r\nI don't know and I don't care.",
    "I went to a Foo Fighters Concert once... It was Everlong...",
    "Why did the sentence fail the driving test? It never came to a full stop.",
    "Some people eat light bulbs. They say it's a nice light snack.",
    "What's the difference between a rooster and a crow? A rooster can crow but a crow cannot rooster.",
    "I went to the store to pick up eight cans of sprite... when I got home I realized I'd only picked seven up",
    "I cut my finger chopping cheese, but I think that I may have grater problems.",
    "Yesterday, I accidentally swallowed some food coloring. The doctor says I’m okay, but I feel like I’ve dyed a little inside.",
    "When people are sad, I sometimes let them colour in my tattoos. Sometimes all they need is a shoulder to crayon.",
    "Last night me and my girlfriend watched three DVDs back to back. Luckily I was the one facing the TV.",
    "I got a reversible jacket for Christmas, I can't wait to see how it turns out.",
    "What do you get when you cross a pig and a pineapple? A porky pine",
    "What did Romans use to cut pizza before the rolling cutter was invented? Lil Caesars",
    "Why did the banana go to the doctor? He was not \"peeling\" well.",
    "My pet mouse 'Elvis' died last night. He was caught in a trap..",
    "Never take advice from electrons. They are always negative.",
    "Why are oranges the smartest fruit? Because they are made to concentrate. ",
    "A girl once asked me what my heart desired, apparently blood, oxygen and neural messages were all wrong answers",
    "Why is it always hot in the corner of a room? Because a corner is 90 degrees.",
    "What did the beaver say to the tree? It's been nice gnawing you.",
    "How do you fix a damaged jack-o-lantern? You use a pumpkin patch.",
    "Why do cows not have toes? They lactose!",
    "What did the late tomato say to the early tomato? I’ll ketch up",
    "I have kleptomania, but when it gets bad, I take something for it.",
    "I used to be addicted to soap, but I'm clean now.",
    "When is a door not a door? When it's ajar.",
    "I made a belt out of watches once... It was a waist of time.",
    "Why did Mozart kill all his chickens?\r\nBecause when he asked them who the best composer was, they'd all say \"Bach bach bach!\"\r\n",
    "This furniture store keeps emailing me, all I wanted was one night stand!",
    "How do you find Will Smith in the snow?  Look for fresh prints.",
    "My sister bet me $15 that I couldn't build a car out of spaghetti. You should have seen the look on her face as I drove pasta.",
    "My boss told me to have a good day... so I went home.",
    "I just read a book about Stockholm syndrome. It was pretty bad at first, but by the end I liked it.",
    "Why do trees seem suspicious on sunny days? Dunno, they're just a bit shady.",
    "If at first you don't succeed, sky diving is not for you!",
    "I'd like to start a diet, but I've got too much on my plate right now.",
    "What did the drummer name her twin daughters? Anna One, Anna Two...",
    "What kind of music do mummy's like? Rap",
    "I remember when I was a kid, I opened my fridge and noticed one of my vegetables were crying. I guess I have some emotional cabbage.",
    "What's large, grey, and doesn't matter?\r\nAn irrelephant.\r\n",
    "Frankenstein enters a bodybuilding competition and finds he has seriously misunderstood the objective.",
    "A book just fell on my head. I only have my shelf to blame.",
    "What did the dog say to the two trees? Bark bark.",
    "I've got a joke about vegetables for you... but it's a bit corny.",
    "If a child refuses to sleep during nap time, are they guilty of resisting a rest?",
    "Why can't your nose be 12 inches long? Because then it'd be a foot!",
    "Have you ever heard of a music group called Cellophane? They mostly wrap.",
    "What do you call a boy who stopped digging holes? Douglas.",
    "What did the mountain climber name his son? Cliff.",
    "Why should you never trust a pig with a secret? Because it's bound to squeal.",
    "Why are mummys scared of vacation? They're afraid to unwind.",
    "Whiteboards ... are remarkable.",
    "What kind of dinosaur loves to sleep? A stega-snore-us.",
    "What has three letters and starts with gas? A Car.",
    "What’s Forest Gump’s Facebook password? 1forest1",
    "What kind of tree fits in your hand? A palm tree!",
    "Whenever the cashier at the grocery store asks my dad if he would like the milk in a bag he replies, ‘No, just leave it in the carton!’",
    "I used to be addicted to the hokey pokey, but I turned myself around.",
    "How many tickles does it take to tickle an octopus? Ten-tickles!",
    "Me: If humans lose the ability to hear high frequency volumes as they get older, can my 4 week old son hear a dog whistle?\r\n\r\nDoctor: No, humans can never hear that high of a frequency no matter what age they are.\r\n\r\nMe: Trick question... dogs can't whistle.",
    "What musical instrument is found in the bathroom? A tuba toothpaste.",
    "I can't take my dog to the pond anymore because the ducks keep attacking him. That's what I get for buying a pure bread dog.",
    "My boss told me to attach two pieces of wood together... I totally nailed it!",
    "What was the pumpkin’s favorite sport?\r\n\r\nSquash.",
    "Recent survey revealed 6 out of 7 dwarf's aren't happy.",
    "What do you call corn that joins the army? Kernel.",
    "I've been trying to come up with a dad joke about momentum . . . but I just can't seem to get it going.",
    "‘Put the cat out’ … ‘I didn’t realize it was on fire",
    "Why don't skeletons ride roller coasters? They don't have the stomach for it.",
    "Is there a hole in your shoe? No… Then how’d you get your foot in it?",
    "Every night at 11:11, I make a wish that someone will come fix my broken clock.",
    "Two muffins were sitting in an oven, and the first looks over to the second, and says, “man, it’s really hot in here”. The second looks over at the first with a surprised look, and answers, “WHOA, a talking muffin!”",
    "What's the difference between a guitar and a fish? You can tune a guitar but you can't \"tuna\" fish!",
    "Did you hear that the police have a warrant out on a midget psychic ripping people off? It reads “Small medium at large.”",
    "Why don't sharks eat clowns?  Because they taste funny.",
    "Just read a few facts about frogs. They were ribbiting.",
    "Two satellites decided to get married. The wedding wasn't much, but the reception was incredible.",
    "What do you call a fish with no eyes? A fsh.",
    "What do you get if you put a duck in a cement mixer? Quacks in the pavement.",
    "They tried to make a diamond shaped like a duck. It quacked under the pressure.",
    "Where’s the bin? Dad: I haven’t been anywhere!",
    "My wife said I was immature. So I told her to get out of my fort.",
    "Why did the knife dress up in a suit? Because it wanted to look sharp",
    "How do you make a water bed more bouncy. You use Spring Water",
    "I considered building the patio by myself. But I didn't have the stones.",
    "In my career as a lumberjack I cut down exactly 52,487 trees. I know because I kept a log.",
    "Why do bears have hairy coats? Fur protection.",
    "What do you get when you cross a bee and a sheep? A bah-humbug.\r\n",
    "What did one snowman say to the other snow man? Do you smell carrot?",
    "\"Dad, do you think it's going to snow this winter?\" \"I dont know, its all up in the air\"",
    "Why do bees hum? Because they don't know the words.",
    "What do you call a troublesome Canadian high schooler? A poutine.",
    "A magician was driving down the street and then he turned into a driveway.",
    "Don't trust atoms. They make up everything.",
    "If you walk into a forest and cut down a tree, but the tree doesn't understand why you cut it down, do you think it's stumped?",
    "Where do bees go to the bathroom?  The BP station.",
    "What is the best way to carve?\r\nWhittle by whittle.",
    "What's a ninja's favorite type of shoes? Sneakers!",
    "Why did the tree go to the dentist? It needed a root canal.",
    "It was raining cats and dogs the other day. I almost stepped in a poodle.",
    "Why do bananas have to put on sunscreen before they go to the beach? Because they might peel!",
    "What do you call a bee that lives in America? A USB.",
    "I was wondering why the frisbee was getting bigger, then it hit me.",
    "A farmer had 297 cows, when he rounded them up, he found he had 300",
    "What's the difference between a hippo and a zippo? One is really heavy, the other is a little lighter.",
    "I’ve got this disease where I can’t stop making airport puns. The doctor says it terminal.",
    "Somebody stole my Microsoft Office and they're going to pay - you have my Word.",
    "What concert costs only 45 cents? 50 cent featuring Nickelback.",
    "I couldn't figure out how the seat belt worked. Then it just clicked.",
    "What did the green grape say to the purple grape?\r\nBREATH!!",
    "What do you call a dad that has fallen through the ice? A Popsicle.",
    "Two parrots are sitting on a perch. One turns to the other and asks, \"do you smell fish?\"",
    "Bad at golf? Join the club.",
    "I had a pair of racing snails. I removed their shells to make them more aerodynamic, but they became sluggish.",
    "What do you call a pile of cats?  A Meowtain.",
    "How do hens stay fit? They always egg-cercise!",
    "Can a kangaroo jump higher than the Empire State Building? Of course. The Empire State Building can't jump.",
    "What do you give a sick lemon? Lemonaid.",
    "What do you call an old snowman? Water.",
    "I tried to milk a cow today, but was unsuccessful. Udder failure.",
    "I just got fired from a florist, apparently I took too many leaves.",
    "Why don’t skeletons ever go trick or treating? Because they have nobody to go with.",
    "What does a female snake use for support? A co-Bra!",
    "\"Dad, I'm cold.\"\r\n\"Go stand in the corner, I hear it's 90 degrees.\"",
    "Child: Dad, make me a sandwich. Dad: Poof! You're a sandwich.",
    "which flower is most fierce? Dandelion",
    "Why are graveyards so noisy? Because of all the coffin.",
    "What kind of bagel can fly? A plain bagel.",
    "How many apples grow on a tree? All of them!",
    "What do you call a careful wolf? Aware wolf.",
    "I was just looking at my ceiling. Not sure if it’s the best ceiling in the world, but it’s definitely up there.",
    "Why do valley girls hang out in odd numbered groups? Because they can't even.",
    "“My Dog has no nose.” “How does he smell?” “Awful”",
    "What do you call a cow with no legs? Ground beef.",
    "Why are snake races so exciting? They're always neck and neck.",
    "Why did the half blind man fall in the well? Because he couldn't see that well!",
    "As I suspected, someone has been adding soil to my garden. The plot thickens.",
    "What do bees do after they are married? They go on a honeymoon.",
    "If I could name myself after any Egyptian god, I'd be Set.",
    "Why doesn't the Chimney-Sweep call out sick from work? Because he's used to working with a flue.",
    "It’s hard to explain puns to kleptomaniacs, because they take everything literally.",
    "It's difficult to say what my wife does, she sells sea shells by the sea shore.",
    "Why did Dracula lie in the wrong coffin? He made a grave mistake.",
    "What did one plate say to the other plate? Dinner is on me!",
    "what do you call a dog that can do magic tricks? a labracadabrador",
    "Doctor: Do you want to hear the good news or the bad news?\r\nPatient: Good news please.\r\nDoctor: we're naming a disease after you.",
    "Atheism is a non-prophet organisation.",
    "I tried to write a chemistry joke, but could never get a reaction.",
    "I gave my friend 10 puns hoping that one of them would make him laugh. Sadly, no pun in ten did.",
    "What do computers and air conditioners have in common? They both become useless when you open windows.",
    "What do you call a monkey in a mine field? A babooooom!\n",
    "Scientists finally did a study on forks. It's about tine!",
    "I cut my finger cutting cheese. I know it may be a cheesy story but I feel grate now.",
    "How do you steal a coat? You jacket.",
    "Why don't you find hippopotamuses hiding in trees?\r\nThey're really good at it.",
    "what happens when you cross a sheep with a kangaroo ? A woolly jumper!",
    "I’m reading a book on the history of glue – can’t put it down.",
    "Want to hear a joke about construction? Nah, I'm still working on it.",
    "My friend told me that pepper is the best seasoning for a roast, but I took it with a grain of salt.",
    "Just watched a documentary about beavers… It was the best damn program I’ve ever seen.",
    "Why do choirs keep buckets handy? So they can carry their tune",
    "Did you hear about the kidnapping at school? It's ok, he woke up.",
    "I asked my date to go to the gym the other day. They never showed up. That's when I knew we wouldn't work out.",
    "You will never guess what Elsa did to the balloon. She let it go.",
    "Did you hear about the two thieves who stole a calendar? They each got six months.",
    "Waking up this morning was an eye-opening experience.",
    "Why can't eggs have love? They will break up too soon.",
    "You can't run through a camp site. You can only ran, because it's past tents.",
    "They're making a movie about clocks. It's about time",
    "I’ve just been reading a book about anti-gravity, it’s impossible to put down!",
    "Archaeology really is a career in ruins.",
    "Have you ever seen fruit preserves being made? It's jarring.",
    "I was going to get a brain transplant, but I changed my mind",
    "Have you heard about the owl sanctuary job opening? It’s all night shifts but they’re all a hoot over there.",
    "I boiled a funny bone last night and had a laughing stock",
    "Why can't you use \"Beef stew\" as a password? Because it's not stroganoff.",
    "Animal Fact #25: Most bobcats are not named bob.",
    "What did the piece of bread say to the knife? Butter me up.",
    "Why couldn't the lifeguard save the hippie? He was too far out, man.",
    "They say Dodger Stadium can hold up to fifty-six thousand people, but that is just a ballpark figure.",
    "Some people say that I never got over my obsession with Phil Collins.\r\nBut take a look at me now.",
    "Why did the girl smear peanut butter on the road? To go with the traffic jam.",
    "It takes guts to be an organ donor.",
    "The rotation of earth really makes my day.",
    "How much does a hipster weigh? An instagram.",
    "A woman is on trial for beating her husband to death with his guitar collection. Judge says, ‘First offender?’ She says, ‘No, first a Gibson! Then a Fender!’",
    "I saw an ad in a shop window, \"Television for sale, $1, volume stuck on full\", I thought, \"I can't turn that down\".",
    "What kind of dog lives in a particle accelerator? A Fermilabrador Retriever.",
    "I always wanted to look into why I procrastinate, but I keep putting it off. ",
    "What's blue and not very heavy?  Light blue.",
    "Guy told me today he did not know what cloning is. I told him, \"that makes 2 of us.\"",
    "I was so proud when I finished the puzzle in six months, when on the side it said three to four years.",
    "Where did you learn to make ice cream? Sunday school.",
    "Coffee has a tough time at my house, every morning it gets mugged.",
    "A quick shoutout to all of the sidewalks out there... Thanks for keeping me off the streets.",
    "Where does Napoleon keep his armies? In his sleevies.",
    "What's the difference between roast beef and pea soup. Anyone can roast beef, but nobody can pee soup.",
    "Leather is great for sneaking around because it's made of hide.",
    "What do you get if you cross a turkey with a ghost? A poultry-geist!",
    "People are making apocalypse jokes like there’s no tomorrow.",
    "What is the tallest building in the world? The library – it’s got the most stories!",
    "What kind of magic do cows believe in? MOODOO.",
    "What’s the longest word in the dictionary? Smiles. Because there’s a mile between the two S’s.",
    "Why don't eggs tell jokes? They'd crack each other up",
    "I just broke my guitar. It's okay, I won't fret",
    "It's only a murder of crows if there's probable caws.",
    "How many kids with ADD does it take to change a lightbulb? Let's go ride bikes!",
    "Where do hamburgers go to dance? The meat-ball.",
    "I invented a new word! Plagiarism!",
    "Did you know that ghosts call their true love their ghoul-friend?",
    "What do you call a cow with two legs? Lean beef.",
    "What did the big flower say to the littler flower? Hi, bud!",
    " I never wanted to believe that my Dad was stealing from his job as a road worker. But when I got home, all the signs were there.\r\n\r\n",
    "Why do pumpkins sit on people’s porches?\r\n\r\nThey have no hands to knock on the door.",
    "Who is the coolest Doctor in the hospital? The hip Doctor!",
    "Why was ten scared of seven? Because seven ate nine.",
    "What do you get when you cross a rabbit with a water hose? Hare spray.",
    "I applied to be a doorman but didn't get the job due to lack of experience. That surprised me, I thought it was an entry level position.",
    "I knew a guy who collected candy canes, they were all in mint condition",
    "A boy dug three holes in the yard. When his mother saw, she exclaimed: \"well, well, well\"",
    "Why do nurses carry around red crayons? Sometimes they need to draw blood.",
    "Why was the shirt happy to hang around the tank top? Because it was armless",
    "Why does a chicken coop only have two doors? Because if it had four doors it would be a chicken sedan.",
    "\"I'll call you later.\" Don't call me later, call me Dad.",
    "Why did the teddy bear say “no” to dessert? Because she was stuffed.",
    "I don't trust sushi, there's something fishy about it.",
    "New atoms frequently lose electrons when they fail to keep an ion them.",
    "Did you hear the one about the giant pickle?  He was kind of a big dill.",
    "Breaking news! Energizer Bunny arrested – charged with battery.",
    "How many bones are in the human hand? A handful of them.",
    "A red and a blue ship have just collided in the Caribbean. Apparently the survivors are marooned.",
    "I've just written a song about a tortilla. Well, it is more of a rap really.",
    "Can February march? No, but April may.",
    "So a duck walks into a pharmacy and says “Give me some chap-stick… and put it on my bill”",
    "Egyptians claimed to invent the guitar, but they were such lyres.﻿",
    "Toasters were the first form of pop-up notifications.",
    "What is a witch's favorite subject in school? Spelling!",
    "What do you call a crowd of chess players bragging about their wins in a hotel lobby? Chess nuts boasting in an open foyer.",
    "Which side of the chicken has more feathers? The outside.",
    "Remember, the best angle to approach a problem from is the \"try\" angle.",
    "Why are fish easy to weigh? Because they have their own scales.",
    "What did the scarf say to the hat? You go on ahead, I am going to hang around a bit longer.",
    "Did you hear about the scientist who was lab partners with a pot of boiling water? He had a very esteemed colleague.",
    "This morning I was wondering where the sun was, but then it dawned on me.",
    "Writing with a broken pencil is pointless.",
    "What did the sea say to the sand? \"We have to stop meeting like this.\"",
    "Why is it so windy inside an arena? All those fans.",
    "A panda walks into a bar and says to the bartender “I’ll have a Scotch and . . . . . . . . . . . . . . Coke thank you”. \r\n\r\n“Sure thing” the bartender replies and asks “but what’s with the big pause?” \r\n\r\nThe panda holds up his hands and says “I was born with them”",
    "I've started telling everyone about the benefits of eating dried grapes. It's all about raisin awareness.",
    "“Doctor, I’ve broken my arm in several places” Doctor “Well don’t go to those places.”",
    "Where was the Declaration of Independence signed?\r\n\r\nAt the bottom! ",
    "What’s the difference between an African elephant and an Indian elephant? About 5000 miles.",
    "Two peanuts were walking down the street. One was a salted",
    "Don’t interrupt someone working intently on a puzzle. Chances are, you’ll hear some crosswords.",
    "Today a man knocked on my door and asked for a small donation towards the local swimming pool. I gave him a glass of water.",
    "What did the Zen Buddist say to the hotdog vendor? Make me one with everything.",
    "Why did the clown have neck pain? - Because he slept funny",
    "What did the digital clock say to the grandfather clock? Look, no hands!",
    "A weasel walks into a bar. The bartender says, \"Wow, I've never served a weasel before. What can I get for you?\"\r\n\"Pop,\" goes the weasel.",
    "How was the snow globe feeling after the storm? A little shaken.",
    "Did you hear the one about the guy with the broken hearing aid? Neither did he.",
    "Did you hear about the campsite that got visited by Bigfoot? It got in tents.",
    "I saw a documentary on TV last night about how they put ships together.  It was rivetting.",
    "What did the Red light say to the Green light? Don't look at me I'm changing!",
    "What did the ocean say to the beach? Thanks for all the sediment.",
    "What did the left eye say to the right eye? Between us, something smells!",
    "What do you call a fly without wings? A walk.",
    "Why did the melons plan a big wedding? Because they cantaloupe!",
    "Yesterday I confused the words \"jacuzzi\" and \"yakuza\". Now I'm in hot water with the Japanese mafia.",
    "What is the least spoken language in the world?\r\nSign Language",
    "What do birds give out on Halloween? Tweets.",
    "I used to think I was indecisive, but now I'm not sure.",
    "Velcro… What a rip-off.",
    "Have you heard the rumor going around about butter? Never mind, I shouldn't spread it.",
    "Every morning when I go out, I get hit by bicycle. Every morning!  It's a vicious cycle.",
    "What happens to a frog's car when it breaks down? It gets toad.",
    "I fear for the calendar, its days are numbered.\n",
    "I'm glad I know sign language, it's pretty handy.",
    "The other day, my wife asked me to pass her lipstick but I accidentally passed her a glue stick. She still isn't talking to me.",
    "What do you get when you cross a chicken with a skunk? A fowl smell!",
    "Where do you take someone who’s been injured in a peek-a-boo accident? To the I.C.U.",
    "As I get older, I think of all the people I lost along the way. Maybe a career as a tour guide wasn't such a good idea.",
    "How many hipsters does it take to change a lightbulb? Oh, it's a really obscure number. You've probably never heard of it.",
    "Where do sheep go to get their hair cut? The baa-baa shop.",
    "Our wedding was so beautiful, even the cake was in tiers.",
    "Why did the miner get fired from his job? He took it for granite...",
    "What did the hat say to the scarf?\r\nYou can hang around. I'll just go on ahead.\r\n",
    "Where do cats write notes?\r\nScratch Paper!",
    "Why is the new Kindle screen textured to look like paper? So you feel write at home.",
    "When my wife told me to stop impersonating a flamingo, I had to put my foot down.",
    "No matter how kind you are, German children are kinder.",
    "What’s the advantage of living in Switzerland? Well, the flag is a big plus.",
    "When I left school, I passed every one of my exams with the exception of Greek Mythology. It always was my achilles elbow",
    "Why did the cookie cry? It was feeling crumby.",
    "What did Yoda say when he saw himself in 4K? \"HDMI\"",
    "How do you make a 'one' disappear? You add a 'g' and it's 'gone'",
    "Me and my mates are in a band called Duvet. We're a cover band.",
    "Where do you learn to make banana splits? At sundae school.",
    "Nurse: Doctor, there's a patient that says he's invisible. Doctor: Well, tell him I can't see him right now!",
    "What was a more important invention than the first telephone? The second one.",
    "What do you get when you cross a snowman with a vampire? Frostbite.",
    "What do you do when your bunny gets wet? You get your hare dryer.",
    "Did you know crocodiles could grow up to 15 feet? But most just have 4.",
    "There are two types of people in this world, those who can extrapolate from incomplete data...",
    "Why did the fireman wear red, white, and blue suspenders? To hold his pants up.",
    "In the news a courtroom artist was arrested today, I'm not surprised, he always seemed sketchy.",
    "I wanted to be a tailor but I didn't suit the job",
    "What do you call someone with no nose? Nobody knows.",
    "What do you call a girl between two posts? Annette.",
    "What do you call a criminal going down the stairs? Condescending",
    "What do you call a fat psychic? A four-chin teller.",
    "I used to be a banker, but I lost interest.",
    "Why can't a bicycle stand on its own? It's two-tired.",
    "What does a pirate pay for his corn? A buccaneer!",
    "Astronomers got tired watching the moon go around the earth for 24 hours. They decided to call it a day.",
    "My dog used to chase people on a bike a lot. It got so bad I had to take his bike away.",
    "I ate a clock yesterday. It was so time consuming.",
    "Two dyslexics walk into a bra.",
    "I been watching a channel on TV that is strictly just about origami — of course it is paper-view.",
    "Milk is also the fastest liquid on earth – its pasteurized before you even see it",
    "Is the pool safe for diving? It deep ends.",
    "Why do scuba divers fall backwards into the water? Because if they fell forwards they’d still be in the boat.",
    "My wife told me to rub the herbs on the meat for better flavor. That's sage advice.",
    "A man was caught stealing in a supermarket today while balanced on the shoulders of a couple of vampires. He was charged with shoplifting on two counts. ",
    "Ben & Jerry's really need to improve their operation. The only way to get there is down a rocky road.",
    "How are false teeth like stars? They come out at night!",
    "What time did the man go to the dentist? Tooth hurt-y.",
    "I went to the zoo yesterday and saw a baguette in a cage. It was bread in captivity.",
    "Did you hear about the cheese factory that exploded in France? There was nothing left but de Brie.",
    "How does a penguin build it’s house? Igloos it together.",
    "What is this movie about? It is about 2 hours long.",
    "Why are pirates called pirates? Because they arrr!",
    "Where does Fonzie like to go for lunch? Chick-Fil-Eyyyyyyyy.",
    "How does a dyslexic poet write? Inverse.",
    "Don't tell secrets in corn fields. Too many ears around.",
    "What did the pirate say on his 80th birthday? Aye Matey!",
    "Why did the A go to the bathroom and come out as an E? Because he had a vowel movement.",
    "Yesterday a clown held a door open for me. I thought it was a nice jester.",
    "Why did the opera singer go sailing? They wanted to hit the high Cs.",
    "Bought a new jacket suit the other day and it burst into flames. Well, it was a blazer",
    "Whats a penguins favorite relative? Aunt Arctica.",
    "Never Trust Someone With Graph Paper...\r\n\r\nThey're always plotting something.",
    "What do you call an elephant that doesn’t matter? An irrelephant.",
    "What do you call a group of disorganized cats? A cat-tastrophe.",
    "What is bread's favorite number?  Leaven.",
    "Why can’t you hear a pterodactyl go to the bathroom? The p is silent.",
    "How do you know if there’s an elephant under your bed? Your head hits the ceiling!",
    "How do you teach a kid to climb stairs? There is a step by step guide.",
    "Where do owls go to buy their baby clothes? The owlet malls.",
    "Why does Norway have barcodes on their battleships? So when they get back to port, they can Scandinavian.",
    "What's the worst part about being a cross-eyed teacher?\r\n\r\nThey can't control their pupils.",
    "What do you call a fashionable lawn statue with an excellent sense of rhythmn? A metro-gnome",
    "Someone broke into my house last night and stole my limbo trophy. How low can you go?",
    "Why did the coffee file a police report? It got mugged.",
    "Mountains aren't just funny, they are hill areas",
    "I was going to learn how to juggle, but I didn't have the balls.",
    "The Swiss must've been pretty confident in their chances of victory if they included a corkscrew in their army knife.",
    "Why was the strawberry sad? Its parents were in a jam.",
    "I wear a stethoscope so that in a medical emergency I can teach people a valuable lesson about assumptions.",
    "Why are ghosts bad liars? Because you can see right through them!",
    "Every machine in the coin factory broke down all of a sudden without explanation. It just doesn’t make any cents.",
    "Why does it take longer to get from 1st to 2nd base, than it does to get from 2nd to 3rd base? Because there’s a Shortstop in between!",
    "What do you do when you see a space man?\r\nPark your car, man.",
    "If you want a job in the moisturizer industry, the best advice I can give is to apply daily.",
    "Where do you take someone who has been injured in a Peek-a-boo accident? To the I.C.U.",
    "When you have a bladder infection, urine trouble.",
    "How do you make Lady Gaga cry? Poker face. ",
    "What do you call a group of killer whales playing instruments? An Orca-stra.",
    "I was in an 80's band called the prevention. We were better than the cure.",
    "What did Michael Jackson name his denim store?    Billy Jeans!",
    "People saying 'boo! to their friends has risen by 85% in the last year.... That's a frightening statistic.",
    "Geology rocks, but Geography is where it's at!",
    "Why does Han Solo like gum? It's chewy!",
    "I was at the library and asked if they have any books on \"paranoia\", the librarian replied, \"yes, they are right behind you\"",
    "Have you heard of the band 1023MB? They haven't got a gig yet.",
    "The urge to sing the Lion King song is just a whim away.",
    "What happens when you anger a brain surgeon? They will give you a piece of your mind.",
    "I needed a password eight characters long so I picked Snow White and the Seven Dwarfs.",
    "I used to work at a stationery store.  But, I didn't feel like I was going anywhere.\r\n\r\nSo, I got a job at a travel agency.  Now, I know I'll be going places.",
    "I used to work in a shoe recycling shop. It was sole destroying.",
    "I used to hate facial hair, but then it grew on me.",
    "R.I.P. boiled water. You will be mist.",
    "Q: What did the spaghetti say to the other spaghetti?\r\nA: Pasta la vista, baby!",
    "The first time I got a universal remote control I thought to myself, \"This changes everything\"",
    "Why is the ocean always blue? Because the shore never waves back.",
    "Why did the feline fail the lie detector test? Because he be lion.",
    "Why did the man put his money in the freezer? He wanted cold hard cash!",
    "I decided to sell my Hoover… well it was just collecting dust.",
    "Why do ducks make great detectives? They always quack the case.",
    "What does a clock do when it's hungry? It goes back four seconds!",
    "What do I look like? A JOKE MACHINE!?",
    "I bought shoes from a drug dealer once. I don't know what he laced them with, but I was tripping all day.",
    "What is a tornado's favorite game to play? Twister!",
    "You know that cemetery up the road? People are dying to get in there.",
    "Pie is $2.50 in Jamaica and $3.00 in The Bahamas. These are the pie-rates of the Caribbean.",
    "What do you call a fish wearing a bowtie? Sofishticated.",
    "Did you hear about the Mexican train killer? He had loco motives",
    "Can I watch the TV? Dad: Yes, but don’t turn it on.",
    "What is worse then finding a worm in your Apple? Finding half a worm in your Apple.",
    "What do vegetarian zombies eat? Grrrrrainnnnnssss.",
    "\"I'm sorry.\" \"Hi sorry, I'm dad\"",
    "What is the hardest part about sky diving? The ground.",
    "Why did the cowboy have a weiner dog? Somebody told him to get a long little doggy.",
    "My friend keeps telling me \"Cheer up. You aren't stuck in a deep hole in the ground, filled with water.\"\r\nI know he means well.",
    "Who did the wizard marry? His ghoul-friend",
    "How many seconds are in a year?\r\n12.\r\nJanuary 2nd, February 2nd, March 2nd, April 2nd.... etc",
    "I ordered a chicken and an egg from Amazon. I'll let you know.",
    "Ever wondered why bees hum? It's because they don't know the words.",
    "How many optometrists does it take to change a light bulb? 1 or 2? 1... or 2?",
    "There's not really any training for garbagemen. They just pick things up as they go.",
    "Did you hear about the cow who jumped over the barbed wire fence? It was udder destruction.",
    "I was shocked when I was diagnosed as colorblind... It came out of the purple.",
    "How come the stadium got hot after the game? Because all of the fans left.",
    "Where does astronauts hangout after work? At the spacebar.",
    "What do you call a bear with no teeth? A gummy bear!",
    "I’ve deleted the phone numbers of all the Germans I know from my mobile phone. Now it’s Hans free.",
    "What do you call your friend who stands in a hole? Phil.",
    "A doll was recently found dead in a rice paddy. It's the only known instance of a nick nack paddy wack.",
    "How do you tell the difference between a crocodile and an alligator? You will see one later and one in a while.",
    "What do you call a fake noodle? An impasta.",
    "The word queue is ironic. It's just q with a bunch of silent letters waiting in line.",
    "What do you call a droid that takes the long way around? R2 detour.",
    "What's the best thing about elevator jokes? They work on so many levels.",
    "Where do rabbits go after they get married? On a bunny-moon.",
    "Why do cows wear bells? Because their horns don't work.",
    "Two fish are in a tank, one turns to the other and says, \"how do you drive this thing?\"",
    "I finally bought the limited edition Thesaurus that I've always wanted. When I opened it, all the pages were blank.\r\nI have no words to describe how angry I am.",
    "This is my step ladder. I never knew my real ladder.",
    "I was thinking about moving to Moscow but there is no point Russian into things.",
    "A man walks into a bar and orders helicopter flavor chips. The barman replies “sorry mate we only do plain”",
    "It's been months since I bought the book \"how to scam people online\". It still hasn't turned up.",
    "Why is it a bad idea to iron your four-leaf clover? Cause you shouldn't press your luck.",
    "I got an A on my origami assignment when I turned my paper into my teacher",
    "What did the fish say when it swam into a wall? Damn!",
    "I accidentally drank a bottle of invisible ink. Now I’m in hospital, waiting to be seen.",
    "Why does Waldo only wear stripes? Because he doesn't want to be spotted.",
    "My New Years resolution is to stop leaving things so late.",
    "Why did the scarecrow win an award? Because he was outstanding in his field.",
    "Americans can't switch from pounds to kilograms overnight. That would cause mass confusion.",
    "An apple a day keeps the bullies away. If you throw it hard enough.",
    "Why does Superman get invited to dinners? Because he is a Supperhero.",
    "Why is no one friends with Dracula? Because he's a pain in the neck.",
    "A man got hit in the head with a can of Coke, but he was alright because it was a soft drink.",
    "What is the leading cause of dry skin? Towels",
    "A man walked in to a bar with some asphalt on his arm. He said “Two beers please, one for me and one for the road.”",
    "Did you know the first French fries weren't actually cooked in France? They were cooked in Greece.",
    "I’ll tell you something about German sausages, they’re the wurst",
    "Where did Captain Hook get his hook? From a second hand store.",
    "I got fired from a florist, apparently I took too many leaves.",
    "Two silk worms had a race. They ended up in a tie.",
    "I got fired from the transmission factor, turns out I didn't put on enough shifts...",
    "Where do young cows eat lunch? In the calf-ateria.",
    "I went to the doctor today and he told me I had type A blood but it was a type O.",
    "How does a French skeleton say hello? Bone-jour.",
    "Why was the big cat disqualified from the race? Because it was a cheetah.",
    "What do prisoners use to call each other? Cell phones.",
    "What’s E.T. short for? He’s only got little legs.",
    "We all know where the Big Apple is but does anyone know where the Minneapolis?",
    "What kind of award did the dentist receive? A little plaque.",
    "Got a new suit recently made entirely of living plants. I wasn’t sure at first, but it’s grown on me",
    "Do you know where you can get chicken broth in bulk? The stock market.",
    "What's orange and sounds like a parrot? A Carrot.",
    "A Sandwich walks into a bar, the bartender says “Sorry, we don’t serve food here”",
    "What do you get hanging from Apple trees? Sore arms.",
    "I thought about going on an all-almond diet. But that's just nuts.",
    "I don’t play soccer because I enjoy the sport. I’m just doing it for kicks.",
    "People are shocked to discover I have a police record but I love their greatest hits!",
    "Today a girl said she recognized me from vegetarian club, but I’m sure I’ve never met herbivore.",
    "How do you organize a space party? You planet.",
    "How do you make holy water? You boil the hell out of it.",
    "A man is washing the car with his son. The son asks...... \"Dad, can’t you just use a sponge?\"",
    "They laughed when I said I wanted to be a comedian – they’re not laughing now.",
    "What does an angry pepper do? It gets jalapeño face.",
    "Don't buy flowers at a monastery. Because only you can prevent florist friars.",
    "Hostess: Do you have a preference of where you sit?\r\nDad: Down.",
    "Did you hear about the submarine industry? It really took a dive...",
    "The biggest knight at King Arthur's round table was Sir Cumference. He acquired his size from eating too much pi.",
    "To the person who stole my anti-depressant pills: I hope you're happy now.",
    "How do you get a baby alien to sleep?  You rocket.",
    "Why do pirates not know the alphabet? They always get stuck at \"C\".",
    "I saw my husband trip and fall while carrying a laundry basket full of ironed clothes. I watched it all unfold.",
    "Did you hear about the chameleon who couldn't change color? They had a reptile dysfunction.",
    "Why did the house go to the doctor? It was having window panes.",
    "I made a playlist for hiking. It has music from Peanuts, The Cranberries, and Eminem. I call it my Trail Mix.",
    "What do you call a dictionary on drugs? High definition.",
    "A Skeleton walked into a bar he said I need a beer and a mop",
    "How do robots eat guacamole? With computer chips.",
    "Today, my son asked \"Can I have a book mark?\" and I burst into tears. 11 years old and he still doesn't know my name is Brian.",
    "When does a joke become a dad joke? When it becomes apparent.",
    "What’s brown and sounds like a bell? Dung!",
    "What has a bed that you can’t sleep in? A river.",
    "Why do crabs never give to charity? Because they’re shellfish.",
    "What do you call a pig with three eyes? Piiig",
    "How do you make a hankie dance? Put a little boogie in it.",
    "Sgt.: Commissar! Commissar! The troops are revolting! Commissar: Well, you’re pretty repulsive yourself.",
    "The other day I was listening to a song about superglue, it’s been stuck in my head ever since.",
    "What don't watermelons get married? Because they cantaloupe.",
    "If you’re struggling to think of what to get someone for Christmas. Get them a fridge and watch their face light up when they open it.",
    "The great thing about stationery shops is they're always in the same place...",
    "Did you hear about the cheese who saved the world? It was Legend-dairy!",
    "What do you call cheese by itself? Provolone.",
    "How do you fix a broken pizza? With tomato paste.",
    "What's red and bad for your teeth? A Brick.",
    "I heard there was a new store called Moderation. They have everything there",
    "A beekeeper was indicted after he confessed to years of stealing at work. They charged him with emBEEzlement",
    "I used to work for a soft drink can crusher. It was soda pressing.",
    "Why did the chicken get a penalty? For fowl play.",
    "When Dad drops a pea off of his plate ‘oh dear I’ve pee’d on the table!",
    "My cat was just sick on the carpet, I don’t think it’s feline well.",
    "Why did the burglar hang his mugshot on the wall? To prove that he was framed!",
    "I dreamed about drowning in an ocean made out of orange soda last night. It took me a while to work out it was just a Fanta sea.",
    "I had a dream that I was a muffler last night. I woke up exhausted!",
    "A dad washes his car with his son. But after a while, the son says, \"why can't you just use a sponge?\"",
    "Doctor you've got you help me, I'm addicted to twitter. Doctor: I don't follow you.",
    "My boss told me to have a good day. So I went home...",
    "Why do we tell actors to “break a leg?” Because every play has a cast.",
    "I broke my finger at work today, on the other hand I'm completely fine.",
    "I went to a book store and asked the saleswoman where the Self Help section was, she said if she told me it would defeat the purpose.",
    "How does a scientist freshen their breath? With experi-mints!",
    "What has ears but cannot hear? A field of corn.",
    "People who don't eat gluten are really going against the grain.",
    "Sore throats are a pain in the neck!",
    "How did Darth Vader know what Luke was getting for Christmas? He felt his presents.",
    "What's the difference between a seal and a sea lion?\r\nAn ion! ",
    "I think circles are pointless.",
    "What did the Dorito farmer say to the other Dorito farmer? Cool Ranch!",
    "A ghost walks into a bar and asks for a glass of vodka but the bar tender says, “sorry we don’t serve spirits”",
    "You know what they say about cliffhangers...",
    "Why do wizards clean their teeth three times a day? To prevent bat breath!",
    "Someone asked me, what's the ninth letter of the alphabet? It was a complete guess, but I was right.",
    "Feeling pretty proud of myself. The Sesame Street puzzle I bought said 3-5 years, but I finished it in 18 months.",
    "A termite walks into a bar and asks “Is the bar tender here?”",
    "Why are fish so smart? Because they live in schools!",
    "How does the moon cut his hair? Eclipse it.",
    "Why did the worker get fired from the orange juice factory? Lack of concentration.",
    "I couldn't get a reservation at the library. They were completely booked.",
    "Dad, can you put my shoes on? I don't think they'll fit me.",
    "How do you get two whales in a car? Start in England and drive West.",
    "What do you call an Argentinian with a rubber toe? Roberto",
    "I tried taking some high resolution photos of local farmland, but they all turned out a bit grainy.",
    "What did the ocean say to the shore? Nothing, it just waved.",
    "I went to the zoo the other day, there was only one dog in it. It was a shitzu.",
    "Someone asked me to name two structures that hold water. I said \"Well dam\"",
    "I gave all my dead batteries away today, free of charge.",
    "Did you hear the news? FedEx and UPS are merging. They’re going to go by the name Fed-Up from now on.",
    "Why didn't the number 4 get into the nightclub? Because he is 2 square.",
    "What do you call a sheep with no legs? A cloud.",
    "Why did the m&m go to school? Because it wanted to be a Smartie!",
    "Did you hear that David lost his ID in prague? Now we just have to call him Dav.",
    "My wife is on a tropical fruit diet, the house is full of stuff. It is enough to make a mango crazy.",
    "Why are basketball players messy eaters? Because they are always dribbling.",
    "Why do mathematicians hate the U.S.? Because it's indivisible.",
    "I knew i shouldn’t have ate that seafood. Because now i’m feeling a little… Eel",
    "Past, present, and future walked into a bar.... It was tense.",
    "Did you hear about the bread factory burning down? They say the business is toast.",
    "My boss told me that he was going to fire the person with the worst posture. I have a hunch, it might be me.",
    "What's black and white and read all over? The newspaper.",
    "Why are skeletons so calm? Because nothing gets under their skin.",
    "Without geometry life is pointless.",
    "“Hold on, I have something in my shoe”  “I’m pretty sure it’s a foot”",
    "Have you heard about the film \"Constipation\", you probably haven't because it's not out yet.",
    "Did you know Albert Einstein was a real person? All this time, I thought he was just a theoretical physicist!",
    "I hate perforated lines, they're tearable.",
    "I asked the surgeon if I could administer my own anesthetic, they said: go ahead, knock yourself out.",
    "Why did the barber win the race? He took a short cut.",
    "Why did the octopus beat the shark in a fight? Because it was well armed.",
    "What did the doctor say to the gingerbread man who broke his leg? Try icing it.",
    "What did the judge say to the dentist? Do you swear to pull the tooth, the whole tooth and nothing but the tooth?",
    "\"What time is it?\" I don't know... it keeps changing.",
    "You can't trust a ladder. It will always let you down",
    "I am so good at sleeping I can do it with my eyes closed!",
    "I wouldn't buy anything with velcro. It's a total rip-off.",
    "What are the strongest days of the week? Saturday and Sunday...the rest are weekdays.",
    "My friend said to me: \"What rhymes with orange\" I said: \"no it doesn't\"",
    "I had a rough day, and then somebody went and ripped the front and back pages from my dictionary. It just goes from bad to worse.",
    "I adopted my dog from a blacksmith. As soon as we got home he made a bolt for the door.",
    "Where does batman go to the bathroom? The batroom.",
    "Some people say that comedians who tell one too many light bulb jokes soon burn out, but they don't know watt they are talking about. They're not that bright.",
    "A bartender broke up with her boyfriend, but he kept asking her for another shot.",
    "What do you call a cow on a trampoline? A milk shake!",
    "What do you call a nervous javelin thrower? Shakespeare.",
    "What do you call an eagle who can play the piano? Talonted!",
    "What do you call a boomerang that won't come back? A stick.",
    "What do you call a duck that gets all A's? A wise quacker.",
    "I used to work for an origami company but they folded.",
    "There’s a new type of broom out, it’s sweeping the nation.",
    "I just wrote a book on reverse psychology. Do not read it!",
    "The shovel was a ground-breaking invention.",
    "Why don’t seagulls fly over the bay? Because then they’d be bay-gulls!",
    "Parallel lines have so much in common. It’s a shame they’ll never meet.",
    "The best time on a clock is 6:30--hands down.",
    "What do you call a magician who has lost their magic? Ian.",
    "Why do fish live in salt water? Because pepper makes them sneeze!",
    "When do doctors get angry? When they run out of patients.",
    "A man tried to sell me a coffin today. I told him that's the last thing I need.",
    "It doesn't matter how much you push the envelope. It will still be stationary.",
    "What did the shy pebble wish for? That she was a little boulder.",
    "Why did the belt go to prison? He held up a pair of pants!",
    "Wife told me to take the spider out instead of killing it... We had some drinks, cool guy, wants to be a web developer.",
    "What cheese can never be yours? Nacho cheese.",
    "What is a vampire's favorite fruit? A blood orange.",
    "A cannibal is someone who is fed up with people.",
    "Why did the cookie cry?\r\nBecause his mother was a wafer so long",
    "Want to hear my pizza joke? Never mind, it's too cheesy.",
    "What did the grape do when he got stepped on? He let out a little wine.",
    "What did the 0 say to the 8? Nice belt.",
    "Why was the picture sent to prison? It was framed.",
    "Two peanuts were walking down the street. One was a salted.",
    "I burned 2000 calories today, I left my food in the oven for too long.",
    "Cosmetic surgery used to be such a taboo subject.\r\nNow you can talk about Botox and nobody raises an eyebrow.",
    "How can you tell a vampire has a cold? They start coffin.",
    "At the boxing match, the dad got into the popcorn line and the line for hot dogs, but he wanted to stay out of the punchline.",
    "\"Hey, dad, did you get a haircut?\" \"No, I got them all cut.\"",
    "Why is there always a gate around cemeteries? Because people are always dying to get in.",
    "I asked a frenchman if he played video games. He said \"Wii\"",
    "My dentist is the best, he even has a little plaque!",
    "So, I heard this pun about cows, but it’s kinda offensive so I won’t say it. I don’t want there to be any beef between us. ",
    "What do Alexander the Great and Winnie the Pooh have in common? Same middle name.",
    "What kind of music do planets listen to? Nep-tunes.",
    "Shout out to my grandma, that's the only way she can hear.",
    "Why was the broom late for the meeting? He overswept.",
    "I went on a date last night with a girl from the zoo. It was great. She’s a keeper.",
    "Why do you never see elephants hiding in trees? Because they're so good at it.",
    "Mahatma Gandhi, as you know, walked barefoot most of the time, which produced an impressive set of calluses on his feet. \r\nHe also ate very little, which made him rather frail and with his odd diet, he suffered from bad breath. \r\nThis made him a super calloused fragile mystic hexed by halitosis.",
    "What do you call an alligator in a vest? An in-vest-igator!",
    "What did celery say when he broke up with his girlfriend? She wasn't right for me, so I really don't carrot all.",
    "Thanks for explaining the word \"many\" to me. It means a lot.",
    "What's brown and sticky? A stick.",
    "What biscuit does a short person like? Shortbread. ",
    "The invention of the wheel was what got things rolling",
    "Why was Santa's little helper feeling depressed? Because he has low elf esteem.\n",
    "Why did Sweden start painting barcodes on the sides of their battleships? So they could Scandinavian.",
    "Want to hear a chimney joke? Got stacks of em! First one's on the house",
    "What do you call a pig that knows karate? A pork chop!",
    "If two vegans are having an argument, is it still considered beef?",
    "My sea sickness comes in waves.",
    "For Valentine's day, I decided to get my wife some beads for an abacus.  It's the little things that count.",
    "What's the worst thing about ancient history class? The teachers tend to Babylon.",
    "My new thesaurus is terrible. In fact, it's so bad, I'd say it's terrible.",
    "What type of music do balloons hate? Pop music!",
    "Do you want a brief explanation of what an acorn is? In a nutshell, it's an oak tree.",
    "How come a man driving a train got struck by lightning? He was a good conductor.",
    "Camping is intense.",
    "Dad died because he couldn't remember his blood type. I will never forget his last words. Be positive.",
    "I’m on a whiskey diet. I’ve lost three days already.",
    "How does Darth Vader like his toast? On the dark side.",
    "Why didn’t the orange win the race? It ran out of juice.",
    "Did you hear about the runner who was criticized? He just took it in stride",
    "What animal is always at a game of cricket? A bat.",
    "Why did the Clydesdale give the pony a glass of water? 
Because he was a little horse!",
    "I started a new business making yachts in my attic this year...the sails are going through the roof",
    "If you think swimming with dolphins is expensive, you should try swimming with sharks--it cost me an arm and a leg!",
    "What did the Buffalo say to his little boy when he dropped him off at school? Bison.",
    "I used to have a job at a calendar factory but I got the sack because I took a couple of days off.",
    "I am terrified of elevators. I’m going to start taking steps to avoid them.",
    "Why do bees have sticky hair? Because they use honey combs!",
    "Did you know you should always take an extra pair of pants golfing? Just in case you get a hole in one.",
    "I wish I could clean mirrors for a living. It's just something I can see myself doing.",
    "How do the trees get on the internet? They log on.",
    "To the guy who invented zero... thanks for nothing.",
    "What lies at the bottom of the ocean and twitches? A nervous wreck.",
    "What is red and smells like blue paint?\r\nRed paint!",
    "*Reversing the car* \"Ah, this takes me back\"",
    "Doctor you've got to help me, I'm addicted to Twitter. Doctor: I don't follow you.",
    "How do locomotives know where they're going? Lots of training",
    "How did the hipster burn the roof of his mouth? He ate the pizza before it was cool.",
    "A butcher accidentally backed into his meat grinder and got a little behind in his work that day.",
    "Did you hear about the new restaurant on the moon? The food is great, but there’s just no atmosphere.",
    "What do you call a beehive without the b's? An eehive.",
    "What kind of pants do ghosts wear? Boo jeans.",
    "Two guys walked into a bar, the third one ducked.",
    "I really want to buy one of those supermarket checkout dividers, but the cashier keeps putting it back.",
    "A horse walks into a bar. The bar tender says \"Hey.\" The horse says \"Sure.\"",
    "Did you hear about the guy who invented Lifesavers? They say he made a mint.",
    "What's the difference between a poorly dressed man on a tricycle and a well dressed man on a bicycle? Attire.",
    "Why are giraffes so slow to apologize? Because it takes them a long time to swallow their pride.",
    "\"Dad, I'm hungry.\" Hello, Hungry. I'm Dad.",
    "I'm practicing for a bug-eating contest and I've got butterflies in my stomach.",
    "I have the heart of a lion... and a lifetime ban from the San Diego Zoo.",
    "What do you call a guy lying on your doorstep? Matt.",
    "I met this girl on a dating site and, I don't know, we just clicked.",
    "What did the calculator say to the student? You can count on me.",
    "What do you call a gorilla wearing headphones? Anything you'd like, it can't hear you.",
    "Have you heard about corduroy pillows?  They're making headlines!"
];
