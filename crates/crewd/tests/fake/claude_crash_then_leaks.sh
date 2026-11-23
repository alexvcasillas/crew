#!/bin/sh
# Writes three stderr lines, leaves a child in its own process group
# that holds stdout and stderr, then fails. The child writes its pid to
# $1 once it has left our group.
echo "first error line" >&2
echo "second error line" >&2
echo "third error line" >&2
perl -e 'setpgrp(0, 0); open(my $f, ">", $ARGV[0]) or die; print $f $$; close($f); exec "sleep", "60"' "$1" &
while [ ! -s "$1" ]; do sleep 0.01; done
exit 2
