#!/bin/bash
# A nested X server with DRI3, for testing rxcom's dma-buf path without the real
# display: a headless mutter (private D-Bus session, Wayland socket
# "rxcom-test", no Xwayland of its own) and a rootful Xwayland on that socket.
#   tools/xwayland.sh start [N] [WxH[@R]]   display :N (default 26), WxH (default 1920x1080),
#                                           R Hz (default mutter's 60)
#   tools/xwayland.sh stop [N]
# The Wayland socket name is passed to Xwayland only; nothing exports it.
set -eu
cmd=${1:-start}
n=${2:-26}
size=${3:-1920x1080}
sock=rxcom-test
dir=${RXCOM_SCRATCH:-${XDG_CACHE_HOME:-$HOME/.cache}/rxcom-test}
mkdir -p "$dir/logs"
mpid=$dir/mutter$n.pid
xpid=$dir/xwayland$n.pid
alive() { [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null; }
case $cmd in
start)
	if alive "$xpid"; then
		echo "Xwayland :$n already running"
		exit 0
	fi
	if [ -e "${XDG_RUNTIME_DIR:?}/$sock" ]; then
		echo "Wayland socket $sock exists already; is another test running?" >&2
		exit 1
	fi
	dbus-run-session -- sh -c "echo \$\$ >'$mpid'; exec mutter --wayland --headless --no-x11 \
		--wayland-display=$sock --virtual-monitor $size" >"$dir/logs/mutter$n.log" 2>&1 &
	for _ in $(seq 100); do [ -S "$XDG_RUNTIME_DIR/$sock" ] && break; sleep 0.1; done
	[ -S "$XDG_RUNTIME_DIR/$sock" ] || { echo "mutter did not come up; see $dir/logs/mutter$n.log" >&2; exit 1; }
	WAYLAND_DISPLAY=$sock Xwayland ":$n" -geometry "${size%@*}" -noreset -nolisten tcp \
		>"$dir/logs/xwayland$n.log" 2>&1 &
	echo $! >"$xpid"
	for _ in $(seq 100); do
		DISPLAY=":$n" xdpyinfo >/dev/null 2>&1 && { echo "Xwayland :$n up on headless mutter"; exit 0; }
		sleep 0.1
	done
	echo "Xwayland :$n did not come up; see $dir/logs/xwayland$n.log" >&2
	exit 1
	;;
stop)
	alive "$xpid" && kill "$(cat "$xpid")"
	sleep 0.5
	alive "$mpid" && kill "$(cat "$mpid")"
	rm -f "$xpid" "$mpid"
	echo "Xwayland :$n and mutter stopped"
	;;
*)
	echo "usage: $0 start|stop [display number]" >&2
	exit 2
	;;
esac
