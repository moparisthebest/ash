#!/bin/sh

curl -H "Accept: application/json" 'https://icanhazdadjoke.com/search?limit=30&page='{1..25} | jq '.results | [.[].joke]' | tr -d '\n' | sed 's/\]\[/,/g' | jq . > dad.json
