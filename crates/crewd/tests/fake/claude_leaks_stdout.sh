#!/bin/sh
# Leaves a child in its own process group that keeps stdout and stderr
# open. The child writes its pid to $1 once it has left our group. Then
# this script exits, or waits to be stopped if $2 is "wait".
perl -e 'setpgrp(0, 0); open(my $f, ">", $ARGV[0]) or die; print $f $$; close($f); exec "sleep", "60"' "$1" &
while [ ! -s "$1" ]; do sleep 0.01; done
if [ "$2" = "wait" ]; then
  exec sleep 60
fi
exit 0
