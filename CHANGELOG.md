# Changelog

## 0.3.0 (2026-10-31)

This is the first public release. rxcom composites an X11 desktop without
effects or a configuration file, gives full-screen games the screen, and zooms
the whole desktop around the pointer.

**Composition.** The X server composes the desktop itself with XRender, as
xcompmgr does, into one screen-sized pixmap: the background, then the mapped
windows from the bottom up, only where something changed. Window contents never
leave the X server. Translucent windows blend as premultiplied alpha, and
shaped windows keep their shape.

**Presenters.** `--present` picks what puts the desktop on the screen. The X
server, the default, copies it onto the screen with XRender, works with any
server and driver, and is the supported presenter. GL and Vulkan flip at vblank
and are experimental: on NVIDIA, GL can stall the X server's switch to another
VT, and Vulkan's flips turn on variable refresh, so it presents at every
refresh and keeps the GPU at its top clocks. rxcom says so in a line of its log
when either is chosen, and `--help` lists the known problems. Where GL or
Vulkan cannot start, the X server presents.

**Zoom.** The root window property `_SCREEN_ZOOM` sets the level, from 1x to
16x in 16.16 fixed point. rxcom eases toward it in log space, keeps the pixel
under the pointer where it is, follows the pointer through XInput 2's raw
motion, and leaves the cursor alone. Nearest filtering keeps every pixel
square; `--zoom-filter linear` smooths the zoom instead.

**Games.** An opaque full-screen window gets the screen to itself after 200 ms
(`--unredirect-delay`, `--no-unredirect`), until anything else needs
compositing or the zoom starts. `_NET_WM_BYPASS_COMPOSITOR` lets a window ask
for the screen, or ask to stay composited.

**New windows.** A window that has just been mapped or resized shows what the
screen showed there until its program has drawn it, rather than whatever the X
server copied into its new pixmap.

**Pacing.** Frames follow the refresh rate of the fastest lit monitor
(`--max-fps` to change it), and rxcom sleeps to the microsecond between them.
Nothing is drawn while the screen saver blanks the screen, while no monitor is
lit, or while another VT is in front, and with the X and GL presenters nothing
at all while the screen does not change.

**Recovery.** A watchdog restarts rxcom when a turn of its loop is stuck for
five seconds, with the X presenter if GL or Vulkan was being set up. GPU errors
set the presenter up again, and a return to the X server's VT repaints
everything.

**Packaging.** `packaging/arch/PKGBUILD` builds the working tree for Arch
Linux; GL's and Vulkan's libraries are optional dependencies. rxcom is released
under the MIT License.

## Earlier versions

These were never released.

- **0.2.0 (2026-10-29)** moved the composition into the X server and added the
  X and Vulkan presenters, then the GL presenter.
- **0.1.0 (2026-10-22)** was a prototype that composed in Vulkan from window
  pixmaps shared through DRI3. [DESIGN.md](DESIGN.md#4-why-the-x-server-composes)
  explains why that did not work out on NVIDIA's X driver.
