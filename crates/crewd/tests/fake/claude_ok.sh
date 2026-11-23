#!/bin/sh
# Prints the fixture file given as $1, one line every 50 ms.
while IFS= read -r line || [ -n "$line" ]; do
  printf '%s\n' "$line"
  sleep 0.05
done < "$1"
