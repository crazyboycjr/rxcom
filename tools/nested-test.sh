#!/bin/bash
# End-to-end test of rxcom on a nested Xvfb. The composited screen is compared
# with the X server's own rendering, ARGB blending and the zoom mapping are
# checked numerically, a full-screen window must get the screen to itself, and
# shutdown must leave the screen as found, also after the X server stalled.
#
#   CARGO_TARGET_DIR=... tools/nested-test.sh [display number, default 23]
#
# With RXCOM_TEST_EXTERNAL=1 the display must already be running (for example
# tools/xwayland.sh) and is left running; the stall check is skipped there.
#
# Build first: cargo build --all-targets. Screenshots and logs go to
# $RXCOM_SCRATCH/shots/run (default $XDG_CACHE_HOME/rxcom-test). The real
# display is never touched.
set -uo pipefail
n=${1:-23}
here=$(cd "$(dirname "$0")" && pwd)
scratch=${RXCOM_SCRATCH:-${XDG_CACHE_HOME:-$HOME/.cache}/rxcom-test}
out=$scratch/shots/run
bin=${CARGO_TARGET_DIR:-$here/../target}/debug
export DISPLAY=:$n
mkdir -p "$out"
rm -f "$out"/*.png "$out"/*.log

fails=0
check() {
	if "$@"; then echo "   ok"; else echo "   FAIL"; fails=$((fails + 1)); fi
}
shot() { import -window root "$out/$1.png"; }
pids=()
rx=
external=${RXCOM_TEST_EXTERNAL:-}
cleanup() {
	[ -n "$rx" ] && kill "$rx" 2>/dev/null
	kill "${pids[@]}" 2>/dev/null
	[ -n "$external" ] || "$here/nested.sh" stop "$n" >/dev/null
}
trap cleanup EXIT

[ -n "$external" ] || "$here/nested.sh" start "$n" || exit 1
magick -size 1920x1080 gradient:'#203050'-'#a07040' -fill white -draw 'circle 960,540 960,700' \
	"$out/wallpaper.png"
feh --no-fehbg --bg-tile "$out/wallpaper.png"
xterm -geometry 80x20+1200+80 -fa Monospace -fs 11 -bg '#1d2021' -fg '#ebdbb2' -T term \
	-e sh -c 'printf "rxcom nested test\n\n"; ls -1 /; exec sleep infinity' &
pids+=($!)
"$bin/examples/argbwin" --geometry 400x300+100+100 --depth 24 --pattern quadrants --name quad24 &
pids+=($!)
# An X border, as xmonad draws them.
"$bin/examples/argbwin" --geometry 200x100+600+80 --depth 24 --color ff808080 --border 4 --name bordered &
pids+=($!)
sleep 1.5
shot ref_below
"$bin/examples/argbwin" --geometry 300x200+300+250 --depth 32 --color 80800000 --name argb50 &
pids+=($!)
"$bin/examples/argbwin" --geometry 500x160+1100+300 --depth 32 --color c0004080 --name argbblue &
pids+=($!)
"$bin/examples/argbwin" --geometry 220x60+150+600 --depth 24 --color ffe0c000 --override-redirect --name menu &
pids+=($!)
sleep 1
# Without a window manager focus follows the pointer, and xterm would redraw its
# cursor whenever the pointer enters it; pin the focus to a window that ignores it.
xdotool windowfocus "$(xdotool search --name '^quad24$')"
sleep 0.3
shot ref_plain

"$bin/rxcom" -v >"$out/rxcom.log" 2>&1 &
rx=$!
sleep 2

echo "1. composited output equals the server's own rendering outside ARGB windows"
shot plain
check python3 "$here/compare.py" "$out/ref_plain.png" "$out/plain.png" \
	--ignore 300,250,300,200 --ignore 1100,300,500,160
echo "   a second instance refuses to start:"
check grep -q "another compositor is running" <("$bin/rxcom" 2>&1)

echo "2. ARGB windows blend as premultiplied alpha"
check python3 "$here/argbcheck.py" "$out/ref_below.png" "$out/plain.png" 300,250,300,200 80800000
check python3 "$here/argbcheck.py" "$out/ref_below.png" "$out/plain.png" 1100,300,500,160 c0004080

echo "3. zoom 2x and 4x around the pointer, then back (nearest, pixel-exact)"
xdotool mousemove 400 330
sleep 0.3
shot zoom1
xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 131072
sleep 1
shot zoom2x
check python3 "$here/zoomcheck.py" "$out/zoom1.png" "$out/zoom2x.png" 2 400 330
xdotool mousemove 1290 125
sleep 0.5
shot zoom2x_moved
check python3 "$here/zoomcheck.py" "$out/zoom1.png" "$out/zoom2x_moved.png" 2 1290 125
xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 262144
sleep 1
shot zoom4x
check python3 "$here/zoomcheck.py" "$out/zoom1.png" "$out/zoom4x.png" 4 1290 125
echo "   16x at the screen's corner (the property asks for 30518x, clamped to 16x):"
xdotool mousemove 1919 0
xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 2000000000
sleep 1.5
shot zoom16x
check python3 "$here/zoomcheck.py" "$out/zoom1.png" "$out/zoom16x.png" 16 1919 0
echo "   2.5x, between whole pixels:"
xdotool mousemove 300 250
xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 163840
sleep 1
shot zoom2_5x
check python3 "$here/zoomcheck.py" "$out/zoom1.png" "$out/zoom2_5x.png" 2.5 300 250
xprop -root -remove _PICOM_ZOOM
sleep 1
shot unzoomed
check python3 "$here/compare.py" "$out/zoom1.png" "$out/unzoomed.png"

echo "4. moving, resizing and restacking"
q=$(xdotool search --name '^quad24$')
a=$(xdotool search --name '^argb50$')
xdotool windowmove "$q" 700 560
xdotool windowsize "$q" 520 300
xdotool windowmove "$a" 900 650
sleep 0.7
shot moved
xdotool windowraise "$q"
sleep 0.5
shot raised
echo "   argb50 over the moved and resized window:"
check python3 "$here/argbcheck.py" "$out/raised.png" "$out/moved.png" 900,650,300,200 80800000

echo "5. damage: an animating client keeps updating"
bar_moved() {
	python3 - "$1" "$2" "$3" <<'PY'
import sys
import numpy as np
from PIL import Image
row = int(sys.argv[3])
rows = [np.asarray(Image.open(p).convert("RGB")).astype(int)[row] for p in sys.argv[1:3]]
def bar(r):
    x = np.nonzero((r[:, 0] == 255) & (r[:, 1] == 255) & (r[:, 2] == 0))[0]
    return int(x.min()) if len(x) else None
b = [bar(r) for r in rows]
print(f"   bar at x={b[0]}, then x={b[1]}")
sys.exit(0 if None not in b and b[0] != b[1] else 1)
PY
}
"$bin/examples/argbwin" --geometry 600x80+200+950 --depth 32 --color ff00a0ff --pattern gradient \
	--animate --name anim &
anim=$!
pids+=($anim)
sleep 1
shot anim1
sleep 0.5
shot anim2
check bar_moved "$out/anim1.png" "$out/anim2.png" 990
kill "$anim"
echo "   an X border changing colour, as xmonad's focus does, is redrawn:"
"$bin/examples/argbwin" --geometry 300x60+1300+700 --depth 24 --color ff808080 --border 5 --blink --name blink &
blink=$!
pids+=($blink)
sleep 1
# Uneven gaps, so the shots do not alias with the 50 ms blinking.
for gap in 0.13 0.07 0.19 0.11 0.23 0.05; do shot "blink$gap"; sleep "$gap"; done
check python3 - "$out"/blink0.*.png <<'PY'
import sys
import numpy as np
from PIL import Image
seen = set()
for p in sys.argv[1:]:
    a = np.asarray(Image.open(p).convert("RGB")).astype(int)
    # The border's four sides, 5 pixels wide around 300x60+1305+705.
    sides = [a[700:705, 1300:1610], a[765:770, 1300:1610], a[700:770, 1300:1305], a[700:770, 1605:1610]]
    colours = {tuple(c) for s in sides for c in s.reshape(-1, 3)}
    if len(colours) != 1 or not colours <= {(0, 0xC0, 0), (0xC0, 0, 0xC0)}:
        print(f"   {p}: border colours {colours}")
        sys.exit(1)
    seen |= colours
print(f"   border colours seen: {sorted(seen)}")
sys.exit(0 if len(seen) == 2 else 1)
PY
kill "$blink"
sleep 0.5

echo "6. a full-screen opaque window gets the screen after 200 ms, and gives it back"
"$bin/examples/argbwin" --geometry 1920x1080+0+0 --depth 24 --color ff406080 --animate \
	--override-redirect --name fullscreen &
fs=$!
pids+=($fs)
sleep 1
check grep -q "unredirecting the screen for the full-screen window" "$out/rxcom.log"
echo "   the window draws to the screen itself (rxcom shows no frames now):"
shot fs1
sleep 0.4
shot fs2
check bar_moved "$out/fs1.png" "$out/fs2.png" 540
echo "   zooming composites again, and the screen goes back to the window after the zoom:"
xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 131072
sleep 0.5
check test "$(grep -c "compositing again" "$out/rxcom.log")" -eq 1
xprop -root -remove _PICOM_ZOOM
sleep 1
check test "$(grep -c "unredirecting the screen" "$out/rxcom.log")" -eq 2
"$bin/examples/argbwin" --geometry 300x200+100+100 --depth 24 --color ffc08000 --override-redirect --name popup &
popup=$!
pids+=($popup)
sleep 1
check test "$(grep -c "compositing again" "$out/rxcom.log")" -eq 2
shot popup
check python3 - "$out/popup.png" <<'PY'
import sys
import numpy as np
from PIL import Image
a = np.asarray(Image.open(sys.argv[1]).convert("RGB")).astype(int)
inside = a[100:300, 100:400]
ok = (inside == [0xc0, 0x80, 0]).all()
print(f"   popup composited above the full-screen window: {ok}")
sys.exit(0 if ok else 1)
PY
kill "$popup" "$fs"
sleep 0.7

echo "7. SIGTERM shuts down cleanly and leaves the screen as found"
shot before_exit
kill -TERM "$rx"
wait "$rx"
status=$?
rx=
echo "   rxcom exited with status $status"
check test "$status" -eq 0
sleep 0.5
shot ref_after
check python3 "$here/compare.py" "$out/ref_after.png" "$out/before_exit.png" --ignore 1100,300,500,160
echo "   the selection is free again; a new instance with --zoom-filter linear, at 3x:"
xdotool mousemove 700 400
"$bin/rxcom" --zoom-filter linear >"$out/rxcom2.log" 2>&1 &
rx=$!
sleep 1.5
shot unzoomed2
xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 196608
sleep 1
shot zoom3x_linear
check python3 "$here/zoomcheck.py" "$out/unzoomed2.png" "$out/zoom3x_linear.png" 3 700 400 linear
xprop -root -remove _PICOM_ZOOM
sleep 0.5
kill -TERM "$rx"
wait "$rx"
rx=
check grep -q "released the screen" "$out/rxcom2.log"
echo "   SIGTERM is honoured while frames render back to back:"
"$bin/examples/argbwin" --geometry 600x80+200+950 --depth 32 --color ff00a0ff --animate --name anim2 &
anim2=$!
pids+=($anim2)
"$bin/rxcom" >"$out/rxcom3.log" 2>&1 &
rx=$!
sleep 1.5
start=$(date +%s%N)
kill -TERM "$rx"
wait "$rx"
status=$?
rx=
echo "   exited with status $status after $((($(date +%s%N) - start) / 1000000)) ms"
check grep -q "released the screen" "$out/rxcom3.log"
kill "$anim2"

if [ -z "$external" ]; then
	echo "8. the X server stops answering for 7 s mid-zoom: the watchdog warns, rxcom"
	echo "   waits it out and carries on"
	"$bin/rxcom" >"$out/rxcom4.log" 2>&1 &
	rx=$!
	sleep 1.5
	shot before_hang
	# Zoomed, rxcom asks for the pointer every 4 ms, so it is soon waiting on X.
	xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 131072
	sleep 0.5
	xpid=$(cat "$scratch/xvfb$n.pid")
	kill -STOP "$xpid"
	sleep 7
	kill -CONT "$xpid"
	sleep 0.5
	check kill -0 "$rx"
	check grep -q "the X server has not answered for" "$out/rxcom4.log"
	xprop -root -remove _PICOM_ZOOM
	sleep 1
	shot after_hang
	check python3 "$here/compare.py" "$out/before_hang.png" "$out/after_hang.png" --ignore 1100,300,500,160
	kill -TERM "$rx"
	wait "$rx"
	status=$?
	rx=
	echo "   then exits with status $status on SIGTERM"
	check test "$status" -eq 0
fi

echo "9. no warnings or errors in the log"
problems=$(grep -E 'WARN|ERROR|VUID|Validation' "$out/rxcom.log")
echo "$problems" | head -5
check test -z "$problems"

echo
if [ "$fails" -eq 0 ]; then echo "all checks passed"; else echo "$fails checks FAILED"; fi
exit $((fails > 0))
