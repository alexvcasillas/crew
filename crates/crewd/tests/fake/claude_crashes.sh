#!/bin/sh
# Writes three lines to stderr and fails.
echo "first error line" >&2
echo "second error line" >&2
echo "third error line" >&2
exit 2
