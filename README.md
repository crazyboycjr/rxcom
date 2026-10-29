# rxcom

An X11 compositor in Rust with a macOS-style desktop zoom. The X server
composes the desktop with XRender; Vulkan presents it, flipped at vblank,
wherever the X server can share the scene through DRI3, and the X server
presents by itself elsewhere. DESIGN.md explains why (section 8) and keeps the
record of the prototype before it.

## Build

Needs Rust 1.88 or newer, libxcb, a Vulkan 1.3 driver and `glslc` (Arch package
`shaderc`).

    cargo build --release

## Run

rxcom refuses to start while another compositor owns the screen, and gives the
screen back on SIGINT or SIGTERM.

    rxcom [-v]           # rxcom --help lists the options

The Vulkan presenter presents at every refresh, even when nothing changed. Its
full-screen swapchain flips, which puts a G-SYNC or FreeSync monitor into
variable refresh, and NVIDIA's driver lets no Vulkan program on X opt out:
presenting only on changes would let the monitor's refresh follow the
desktop's pauses, so the cursor would move at the monitor's lowest rate, and
some monitors black out as variable refresh switches on and off. An unchanged
frame is presented again without drawing anything. Nothing is presented while
a game has the screen (it keeps its own variable refresh), while the screen
saver blanks the screen, while no monitor is lit or while another VT is in
front. `--no-continuous` presents only on changes.

Zoom through the root property `_SCREEN_ZOOM`, which xcompmgr-zoom reads too:

    xprop -root -f _SCREEN_ZOOM 32c -set _SCREEN_ZOOM 131072   # 2x
    xprop -root -remove _SCREEN_ZOOM

The property is CARDINAL, format 32, one value on each screen's root window:
the target magnification of that screen in 16.16 fixed point. No property, or a
value of 65536 or less, means no zoom, and values above 16x are clamped. The
compositor eases toward the target (40 ms time constant in log space), keeps the
centre of the pixel under the pointer in place, so input needs no transform, and
leaves the cursor unmagnified. A window manager or any other client sets it.

## Test

Everything runs on nested X servers, never on the real display.

    cargo test --lib
    cargo build --all-targets
    tools/nested-test.sh                            # Xvfb: the X presenter
    tools/stress-test.sh 24 500                     # random operations under openbox
    tools/xwayland.sh start 26                      # a headless Xwayland with DRI3
    RXCOM_TEST_EXTERNAL=1 tools/nested-test.sh 26   # there: the Vulkan presenter
    RXCOM_TEST_EXTERNAL=1 tools/stress-test.sh 26 400
    tools/xwayland.sh stop 26

`VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation` adds the validation layers;
the scripts count their messages as failures. Screenshots and logs go to
`$XDG_CACHE_HOME/rxcom-test` (override with `RXCOM_SCRATCH`).
