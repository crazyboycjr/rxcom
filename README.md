# rxcom

An X11 compositor in Rust on Vulkan, with a macOS-style desktop zoom. This is
the prototype; DESIGN.md describes what it does, what we learned, and the
proposal for the full version.

## Build

Needs Rust 1.85 or newer, libxcb, a Vulkan 1.3 driver with
VK_KHR_push_descriptor, and `glslc` (Arch package `shaderc`).

    cargo build --release

## Run

rxcom refuses to start while another compositor owns the screen.

    rxcom [--display :N] [--gpu NAME] [--no-dri3] [--no-sync-fence]
          [--max-fps N] [--background RRGGBB] [--validation] [-v|-vv]

Zoom through the root property shared with the patched picom (16.16 fixed
point; 65536 or less, or no property, means no zoom):

    xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 131072   # 2x
    xprop -root -remove _PICOM_ZOOM

## Test

Everything runs on nested X servers, never on the real display.

    cargo test --lib
    cargo build --all-targets
    tools/nested-test.sh        # end-to-end checks on a nested Xvfb
    tools/stress-test.sh 24 500 # random window operations under openbox
    tools/xwayland.sh start 26  # a DRI3-capable nested server, then:
    RXCOM_TEST_EXTERNAL=1 tools/nested-test.sh 26
    tools/xwayland.sh stop 26

The scripts use the NVIDIA Vulkan driver unless `VK_DRIVER_FILES` says
otherwise, and write screenshots and logs under
`~/.cache/claude-scratch/rxcom` (override with `RXCOM_SCRATCH`).
`examples/dri3_import.rs` checks the DRI3 import path on a real X server; read
its header before running it.
