#!/bin/sh
# Writes a non-UTF-8 stderr line, then a normal one, and fails.
printf 'bad \377 byte\n' >&2
echo "after bad line" >&2
exit 3
