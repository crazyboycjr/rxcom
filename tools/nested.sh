#!/bin/bash
# Starts or stops a nested Xvfb for testing rxcom without touching the real display.
#   tools/nested.sh start [N] [WxH]   Xvfb :N (default 21), WxH (default 1920x1080), depth 24, Composite, no reset
#   tools/nested.sh stop [N]
# State lives in $RXCOM_SCRATCH (default $XDG_CACHE_HOME/rxcom-test).
set -eu
cmd=${1:-start}
n=${2:-21}
size=${3:-1920x1080}
dir=${RXCOM_SCRATCH:-${XDG_CACHE_HOME:-$HOME/.cache}/rxcom-test}
mkdir -p "$dir/logs"
pidfile="$dir/xvfb$n.pid"
case $cmd in
start)
	if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
		echo "Xvfb :$n already running (pid $(cat "$pidfile"))"
		exit 0
	fi
	Xvfb ":$n" -screen 0 ${size}x24 +extension Composite -nolisten tcp -noreset \
		>"$dir/logs/xvfb$n.log" 2>&1 &
	echo $! >"$pidfile"
	for _ in $(seq 50); do
		DISPLAY=":$n" xdpyinfo >/dev/null 2>&1 && { echo "Xvfb :$n up (pid $!)"; exit 0; }
		sleep 0.1
	done
	echo "Xvfb :$n did not come up" >&2
	exit 1
	;;
stop)
	[ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true
	rm -f "$pidfile"
	echo "Xvfb :$n stopped"
	;;
*)
	echo "usage: $0 start|stop [display number]" >&2
	exit 2
	;;
esac
