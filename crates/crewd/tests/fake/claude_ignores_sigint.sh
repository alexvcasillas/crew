#!/bin/sh
# Ignores SIGINT and SIGTERM so only SIGKILL stops it.
trap '' INT TERM
sleep 60
