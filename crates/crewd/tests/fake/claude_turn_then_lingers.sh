#!/bin/sh
# Prints the fixture file $1, starts a long background child that writes
# its pid to $2, then waits a long time, like a CLI that waits for a
# background task after its turn.
cat "$1"
sleep 600 &
printf '%s' "$!" > "$2"
sleep 600
