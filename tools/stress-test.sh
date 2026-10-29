#!/bin/bash
# Stress test on a nested Xvfb with a reparenting window manager (openbox with
# its stock config, never the user's): random moves, resizes, raises,
# iconifies, withdraws (the window manager hands the client back to the root),
# kills, zoom changes and wallpaper changes while rxcom runs. Afterwards the
# composited screen must equal the server's own rendering and the log must
# hold no warnings or errors.
#
#   CARGO_TARGET_DIR=... tools/stress-test.sh [display number] [steps]
#
# With RXCOM_TEST_EXTERNAL=1 the display must already be running (for example
# tools/xwayland.sh, so rxcom presents with Vulkan) and is left running.
set -uo pipefail
n=${1:-24}
steps=${2:-300}
here=$(cd "$(dirname "$0")" && pwd)
scratch=${RXCOM_SCRATCH:-${XDG_CACHE_HOME:-$HOME/.cache}/rxcom-test}
out=$scratch/shots/stress
bin=${CARGO_TARGET_DIR:-$here/../target}/debug
export DISPLAY=:$n
mkdir -p "$out" "$scratch/xdg"
rm -f "$out"/*.png "$out"/*.log
RANDOM=42

pids=()
rx=
cleanup() {
	[ -n "$rx" ] && kill "$rx" 2>/dev/null
	kill "${pids[@]}" 2>/dev/null
	[ -n "${RXCOM_TEST_EXTERNAL:-}" ] || "$here/nested.sh" stop "$n" >/dev/null
}
trap cleanup EXIT

[ -n "${RXCOM_TEST_EXTERNAL:-}" ] || "$here/nested.sh" start "$n" || exit 1
magick -size 1920x1080 gradient:'#203050'-'#a07040' "$out/wall1.png"
magick -size 1920x1080 gradient:'#305020'-'#4070a0' -fill black -draw 'circle 400,400 400,500' "$out/wall2.png"
feh --no-fehbg --bg-tile "$out/wall1.png"
XDG_CONFIG_HOME=$scratch/xdg openbox --config-file /etc/xdg/openbox/rc.xml >"$out/openbox.log" 2>&1 &
pids+=($!)
sleep 1
"$bin/rxcom" -v >"$out/rxcom.log" 2>&1 &
rx=$!
sleep 1.5

spawn() {
	local i=$1
	local x=$((RANDOM % 1500)) y=$((RANDOM % 800)) w=$((150 + RANDOM % 500)) h=$((100 + RANDOM % 400))
	case $((i % 4)) in
	0) xterm -geometry 60x15+$x+$y -T "s$i" -e sh -c "seq $i $((i + 30)); exec sleep infinity" & ;;
	1) "$bin/examples/argbwin" --geometry ${w}x$h+$x+$y --depth 24 --pattern quadrants --name "s$i" & ;;
	2) "$bin/examples/argbwin" --geometry ${w}x$h+$x+$y --depth 32 --color 80204060 --pattern gradient --name "a$i" & ;;
	3) "$bin/examples/argbwin" --geometry ${w}x$h+$x+$y --depth 24 --color ff$(printf %06x $((RANDOM * 512))) --animate --name "s$i" & ;;
	esac
	pids+=($!)
	eval "pid_$i=$!"
}
for i in $(seq 0 11); do spawn "$i"; done
sleep 2
rss_start=$(awk '/VmRSS/ {print $2}' /proc/"$rx"/status)

echo "running $steps random operations"
for step in $(seq "$steps"); do
	ids=($(xdotool search --name '^[sa][0-9]+$' 2>/dev/null))
	[ ${#ids[@]} -eq 0 ] && { spawn $((100 + step)); continue; }
	w=${ids[$((RANDOM % ${#ids[@]}))]}
	case $((RANDOM % 12)) in
	0 | 1) xdotool windowmove "$w" $((RANDOM % 1700 - 100)) $((RANDOM % 900 - 50)) 2>/dev/null ;;
	2 | 3) xdotool windowsize "$w" $((80 + RANDOM % 900)) $((60 + RANDOM % 600)) 2>/dev/null ;;
	4) xdotool windowactivate "$w" 2>/dev/null ;;
	5) xdotool windowminimize "$w" 2>/dev/null ;;
	6) xdotool windowunmap "$w" 2>/dev/null ;;
	7) for u in $(xdotool search --onlyvisible=0 --name '^[sa][0-9]+$' 2>/dev/null); do xdotool windowmap "$u" 2>/dev/null; done ;;
	8) kill "$(xdotool getwindowpid "$w" 2>/dev/null)" 2>/dev/null; spawn $((100 + step)) ;;
	9) xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM $((65536 + RANDOM * 8)); xdotool mousemove $((RANDOM % 1920)) $((RANDOM % 1080)) ;;
	10) xprop -root -remove _PICOM_ZOOM ;;
	11) feh --no-fehbg --bg-tile "$out/wall$((1 + RANDOM % 2)).png" ;;
	esac
	sleep 0.0$((RANDOM % 6))
	if ! kill -0 "$rx" 2>/dev/null; then
		echo "rxcom died at step $step"
		tail -20 "$out/rxcom.log"
		exit 1
	fi
done

echo "settling: unzoom, map everything, stop animations and ARGB clients"
xprop -root -remove _PICOM_ZOOM
for u in $(xdotool search --name '^[sa][0-9]+$' 2>/dev/null); do xdotool windowmap "$u" 2>/dev/null; done
for p in $(pgrep -x argbwin); do
	if tr '\0' ' ' </proc/"$p"/cmdline | grep -qE -- '--animate|--depth 32'; then kill "$p"; fi
done
xdotool mousemove 5 5
sleep 2
import -window root "$out/composited.png"
rss_end=$(awk '/VmRSS/ {print $2}' /proc/"$rx"/status)
echo "rxcom RSS: $rss_start kB after start-up, $rss_end kB after the run"
echo "windows added $(grep -c ' add 0x' "$out/rxcom.log"), forgotten $(grep -c 'forget 0x' "$out/rxcom.log");" \
	"zoom targets $(grep -c 'zoom target' "$out/rxcom.log"); backgrounds $(grep -c "background: root pixmap" "$out/rxcom.log")"
kill -TERM "$rx"
wait "$rx"
status=$?
rx=
sleep 1
import -window root "$out/uncomposited.png"
echo "rxcom exit status $status"
python3 "$here/compare.py" "$out/uncomposited.png" "$out/composited.png"
same=$?
problems=$(grep -E "WARN|ERROR|VUID|Validation" "$out/rxcom.log")
echo "${problems:-no warnings or errors in the log}" | head -10
[ "$status" -eq 0 ] && [ "$same" -eq 0 ] && [ -z "$problems" ]
