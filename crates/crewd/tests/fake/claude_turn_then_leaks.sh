#!/bin/sh
# Prints the fixture file $2, leaves a child in its own process group
# that holds stdout and stderr, then exits 0. The child writes its pid
# to $1 once it has left our group.
cat "$2"
perl -e 'setpgrp(0, 0); open(my $f, ">", $ARGV[0]) or die; print $f $$; close($f); exec "sleep", "60"' "$1" &
while [ ! -s "$1" ]; do sleep 0.01; done
exit 0
