use anyhow::Result;
use die::{die, Die};
use futures::stream::StreamExt;
use rusqlite::Connection;
use rustkov::prelude::Brain;
use serde_derive::Deserialize;
use std::{
    collections::HashMap, convert::TryFrom, env::args, fs::File, io::Read, iter::Iterator,
    path::Path,
};
use tokio_xmpp::AsyncClient as Client;
use xmpp_parsers::{
    message::{Body, Message, MessageType},
    muc::{muc::History, Muc},
    presence::{Presence, Type as PresenceType},
    BareJid, Element, FullJid, Jid,
};

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

struct Room {
    nick: String,
    chain_indices: Vec<usize>,
    jid: FullJid,
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
            Room {
                nick,
                jid,
                chain_indices,
            },
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
                                    rooms.get(&(node.to_string(), domain.to_string()))
                                {
                                    let nick = &room.nick;
                                    if resource == nick {
                                        continue;
                                    }
                                    let body = &body.0;
                                    println!("from: '{from}', body: {body}");
                                    if body.starts_with(nick) {
                                        let body = body.trim_start_matches(nick);
                                        let body = body.trim_start_matches([',', ':', ' ']);
                                        println!("self body: {body}");
                                        let from = Jid::Bare(BareJid {
                                            node: Some(node.to_string()),
                                            domain: domain.to_string(),
                                        });
                                        if body == "words" {
                                            let stats = brain[room.chain_indices[0]].stats();
                                            let body = format!(
                                                "I know {} words!",
                                                stats.get_total_words()
                                            );
                                            println!("words: {}", body);
                                            // todo: reply to from or just node+domain ?
                                            client
                                                .send_stanza(make_reply(from.clone(), &body))
                                                .await?;
                                        } else if let Some(body) =
                                            brain[room.chain_indices[0]].generate(body)?
                                        {
                                            println!("generated: {}", body);
                                            // todo: reply to from or just node+domain ?
                                            client
                                                .send_stanza(make_reply(from.clone(), &body))
                                                .await?;
                                        }
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
