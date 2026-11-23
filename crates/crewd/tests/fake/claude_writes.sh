#!/bin/sh
# Writes file.txt in its working folder, then prints the fixture given as $1.
printf 'written by the agent\n' > file.txt
exec "$(dirname "$0")/claude_ok.sh" "$1"
