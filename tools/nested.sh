#!/bin/bash
# Starts or stops a nested Xvfb for testing rxcom without touching the real display.
#   tools/nested.sh start [N]   Xvfb :N (default 21), 1920x1080x24, Composite, no reset
#   tools/nested.sh stop [N]
# State lives in $RXCOM_SCRATCH (default ~/.cache/claude-scratch/rxcom).
set -eu
cmd=${1:-start}
n=${2:-21}
dir=${RXCOM_SCRATCH:-$HOME/.cache/claude-scratch/rxcom}
mkdir -p "$dir/logs"
pidfile="$dir/xvfb$n.pid"
case $cmd in
start)
	if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
		echo "Xvfb :$n already running (pid $(cat "$pidfile"))"
		exit 0
	fi
	Xvfb ":$n" -screen 0 1920x1080x24 +extension Composite -nolisten tcp -noreset \
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
