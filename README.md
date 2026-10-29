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

Zoom through the root property shared with picom-zoom (16.16 fixed point;
65536 or less, or no property, means no zoom):

    xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 131072   # 2x
    xprop -root -remove _PICOM_ZOOM

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
