#!/bin/bash
# End-to-end test of rxcom on a nested Xvfb. The composited screen is compared
# with the X server's own rendering, ARGB blending and the zoom mapping are
# checked numerically, and shutdown must leave the screen as found.
#
#   CARGO_TARGET_DIR=... tools/nested-test.sh [display number, default 23]
#
# With RXCOM_TEST_EXTERNAL=1 the display must already be running (for example
# tools/xwayland.sh) and is left running.
#
# Build first: cargo build --all-targets. Screenshots and logs go to
# $RXCOM_SCRATCH/shots/run (default ~/.cache/claude-scratch/rxcom). Vulkan
# runs on the NVIDIA driver unless VK_DRIVER_FILES says otherwise; the real
# display is never touched.
set -uo pipefail
n=${1:-23}
here=$(cd "$(dirname "$0")" && pwd)
scratch=${RXCOM_SCRATCH:-$HOME/.cache/claude-scratch/rxcom}
out=$scratch/shots/run
bin=${CARGO_TARGET_DIR:-$here/../target}/debug
export DISPLAY=:$n
export VK_DRIVER_FILES=${VK_DRIVER_FILES:-/usr/share/vulkan/icd.d/nvidia_icd.json}
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

"$bin/rxcom" -v --validation >"$out/rxcom.log" 2>&1 &
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

echo "3. zoom 2x and 4x around the pointer, then back"
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
echo "   16x (the property asks for 30518x, clamped to 16x):"
xdotool mousemove 300 250
xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 2000000000
sleep 1.5
shot zoom16x
check python3 "$here/zoomcheck.py" "$out/zoom1.png" "$out/zoom16x.png" 16 300 250
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
"$bin/examples/argbwin" --geometry 600x80+200+950 --depth 32 --color ff00a0ff --pattern gradient \
	--animate --name anim &
anim=$!
pids+=($anim)
sleep 1
shot anim1
sleep 0.5
shot anim2
check python3 - "$out/anim1.png" "$out/anim2.png" <<'PY'
import sys
import numpy as np
from PIL import Image
rows = [np.asarray(Image.open(p).convert("RGB")).astype(int)[990, 200:800] for p in sys.argv[1:3]]
def bar(r):
    x = np.nonzero((r[:, 0] == 255) & (r[:, 1] == 255) & (r[:, 2] == 0))[0]
    return int(x.min()) + 200 if len(x) else None
b = [bar(r) for r in rows]
print(f"   bar at x={b[0]}, then x={b[1]}")
sys.exit(0 if None not in b and b[0] != b[1] else 1)
PY
kill "$anim"
sleep 0.5

echo "6. SIGTERM shuts down cleanly and leaves the screen as found"
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
"$bin/rxcom" >"$out/rxcom2.log" 2>&1 &
rx=$!
sleep 1.5
kill -TERM "$rx"
wait "$rx"
rx=
echo "   the selection is free again (a new instance starts and stops):"
check grep -q "released the screen" "$out/rxcom2.log"
echo "   SIGTERM is honoured while frames render back to back:"
"$bin/examples/argbwin" --geometry 600x80+200+950 --depth 32 --color ff00a0ff --animate --name anim2 &
pids+=($!)
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

echo "7. no Vulkan validation messages, warnings or errors in the log"
# A pixmap the server cannot export (Xwayland's root pixmap, compressed modifiers)
# falls back to MIT-SHM copies with one warning; that is handled, not a failure.
problems=$(grep -E 'vulkan:|WARN|ERROR' "$out/rxcom.log" | grep -v 'DRI3 import of pixmap')
echo "$problems" | head -5
check test -z "$problems"

echo
if [ "$fails" -eq 0 ]; then echo "all checks passed"; else echo "$fails checks FAILED"; fi
exit $((fails > 0))
