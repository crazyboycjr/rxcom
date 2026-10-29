# rxcom design

rxcom is an X11 compositor written in Rust. The goal is a compositor that
replaces xcompmgr and picom on the daily desktop, with the mainstream features
of picom and xcompmgr plus a macOS-style desktop zoom. Sections 1 to 7 describe
the 10-22 prototype, what building it taught us and the proposal made then; they
are kept as the record. Section 8 is the M2 design, decided and built on 10-29:
it describes rxcom as it is now.

## 1. The prototype

### 1.1 What it does

rxcom takes the `_NET_WM_CM_Sn` selection and refuses to start when another
compositor holds it. It then redirects the root's children manually and draws
into the Composite Overlay Window, whose input shape is emptied so that every
click falls through to the windows below. It tracks the top-level windows from
an initial QueryTree, taken under a server grab, and afterwards from CreateNotify,
DestroyNotify, MapNotify, UnmapNotify, ConfigureNotify (geometry, and stacking
through `above_sibling`), ReparentNotify, CirculateNotify and GravityNotify.

Every window has a Damage object at the NonEmpty level, which is subtracted on
each notification. Each frame repaints the whole screen. Window pixels reach the
GPU by one of two routes, chosen per pixmap:

- **DRI3 import.** The named window pixmap is exported with BuffersFromPixmap and
  imported as a VkImage with its explicit DRM modifier. The GPU then samples the
  X server's memory in place, with no copy.
- **MIT-SHM copies.** When the server has no DRI3, or cannot export a particular
  pixmap, rxcom copies it with ShmGetImage into a memfd segment and uploads it
  after each damage event.

Depth-32 windows blend as premultiplied alpha. Depth-24 windows are opaque: their
undefined top byte is replaced by one through the image view's swizzle. The
background is the pixmap named by `_XROOTPMAP_ID` or `ESETROOT_PMAP_ID`, tiled,
or a solid colour. Frames are presented through a FIFO swapchain on the overlay
window. Zoom follows the root property `_PICOM_ZOOM`, the same protocol the
patched picom uses (section 1.5). On SIGINT, SIGTERM or SIGHUP rxcom tears down
the GPU side, unredirects, releases the overlay and the selection, and leaves the
screen as it found it.

### 1.2 How it was verified

All functional testing ran on nested X servers; the real display was used only
for the DRI3 component test described in 2.1.

`tools/nested-test.sh` starts an Xvfb, a wallpaper, an xterm and test windows
(`examples/argbwin.rs`), and runs rxcom with the Vulkan validation layers on the
NVIDIA driver. It checks the following, and all of it passes:

- The composited screen equals the X server's own rendering pixel for pixel,
  outside the ARGB windows.
- ARGB windows match the premultiplied "over" operator within one level (in
  practice exactly).
- Zoom at 2x, 4x and 16x matches the documented mapping, computed independently
  in Python, within 0.55 levels. Unzooming restores the exact image.
- After moving, resizing and restacking, the result is exact.
- An animating client keeps updating through damage.
- A second instance refuses to start.
- SIGTERM leaves the screen identical to the uncomposited one and frees the
  selection, including mid-animation, where shutdown takes under 100 ms.
- The log holds no warnings or validation messages.

`tools/stress-test.sh` runs openbox (stock config) as a reparenting window
manager. It drives 1500 random operations: moves, resizes, raises, iconifies,
withdraws (which hand clients back to the root), kills, zoom changes and
wallpaper changes. The final composited screen still equals the server's
rendering exactly, and the log stays clean.

Memory stays flat (about 120 MB RSS, most of it the NVIDIA driver) over 1500
resizes of one window. Idle CPU use is zero.

`tools/xwayland.sh` provides a DRI3-capable nested server: a headless mutter on
a private D-Bus session, with a rootful Xwayland 24.1 on a private Wayland
socket. The same end-to-end checks pass there too. Section 2.3 explains why
windows still go through the copy route on that server.

### 1.3 Code layout

The compositor is 2,663 lines of Rust and 48 of GLSL, about 110 of the Rust
lines being unit tests. The dependencies are x11rb (through libxcb), ash, libc,
log and anyhow.

| File | Lines | Role |
| --- | ---: | --- |
| `src/main.rs` | 95 | options, signalfd, start-up |
| `src/x.rs` | 234 | connection, extensions, selection, overlay, scan, X Sync fence |
| `src/windows.rs` | 177 | the stacking list of top-levels and their resources |
| `src/compositor.rs` | 829 | event handling, pixmap naming, copies, frame recording, the loop |
| `src/shm.rs` | 91 | MIT-SHM (memfd) and plain GetImage reads |
| `src/zoom.rs` | 109 | the zoom property, easing and mapping |
| `src/gpu/mod.rs` | 326 | instance, device selection, queue, helpers |
| `src/gpu/texture.rs` | 276 | dma-buf import, upload images, foreign-queue barriers |
| `src/gpu/render.rs` | 368 | the quad and zoom pipelines, offscreen readback for tests |
| `src/gpu/present.rs` | 89 | the swapchain on the overlay window |
| `shaders/` | 48 | `quad.vert/frag` (textured rectangles), `zoom.vert/frag` |

Tests and tools live outside the binary:

- `examples/argbwin.rs` is a test client with ARGB or opaque windows, known
  pixels, animation and override-redirect.
- `examples/dri3_import.rs` is the DRI3 component test.
- `tools/` holds the nested-server scripts and the pixel checkers.

### 1.4 How a frame is made

The program runs one thread and one loop. Each turn reads the signalfd, drains X
events into the window list, and renders if anything is dirty, at most 240
frames a second. Otherwise it sleeps in `poll()` on the X connection and the
signalfd. While zoomed it wakes every 8 ms to poll the pointer.

A frame then proceeds in order:

1. Wait for the frame slot's fence (two slots), and destroy resources retired
   two frames ago.
2. Name new pixmaps for windows that were mapped or resized, and import or copy
   their contents.
3. If a DRI3-backed window was damaged, wait on an X Sync fence (2.2).
4. Acquire a swapchain image.
5. Record the commands:
   - copy staged pixels into upload images;
   - acquire every imported image from the foreign queue family;
   - draw the background and the windows bottom to top as textured quads, with
     push descriptors and push constants;
   - release the imported images.
6. Submit and present.

Without zoom the quads go straight into the swapchain image. With zoom they go
into an intermediate scene image, and a full-screen pass samples it. Vulkan
presents through a second libxcb connection, so the driver's presentation
traffic never interleaves with rxcom's event stream.

### 1.5 Zoom

The window manager writes the target zoom to the root property `_PICOM_ZOOM`
(CARDINAL, format 32, one value in 16.16 fixed point). A missing property, or a
value of at most 65536, means no zoom. Values are clamped to 1.0 to 16.0. The
current zoom eases exponentially toward the target with a 50 ms time constant
and snaps when within 0.001. The clock starts when the target changes, so a long
idle period never produces a jump.

With pointer position p and zoom z, the viewport origin is o = p·(1 − 1/z), and
desktop point q is drawn at (q − o)·z. The zoom pass computes the inverse for
each screen pixel s, sampling q = (s + 0.5)/z + o with linear filtering. The
desktop point under the hardware cursor therefore never moves, input needs no
transform, and the hardware cursor is left alone.

The pointer is read with QueryPointer once per frame while zoomed or animating,
and every 8 ms while zoomed and idle. A frame is rendered only when the pointer
moved or the zoom changed.

## 2. What the prototype taught us

### 2.1 DRI3 on the NVIDIA X server

These measurements come from `examples/dri3_import.rs` on the real display
(Xorg 21.1.24, NVIDIA 610.57.04, RTX 4070 SUPER). The test used an off-screen
override-redirect window with a child it redirected itself, never the
compositing selection.

- **Modifier.** BuffersFromPixmap returns one plane with modifier
  0x0300000000606014 (NVIDIA block-linear, uncompressed) for depth 24 and depth
  32 alike.
- **Suballocation.** Small pixmaps are suballocated. Every export's dma-buf names
  the whole buffer object, 640 MB to 950 MB during testing, and the pixmap sits
  at an offset inside it (262144, 991232, 55126016 and so on). A 1920x1080 pixmap
  had a buffer of its own, 8.8 MB at offset 0.
- **The plane-offset bug.** Putting that offset into the plane layout of
  `VkImageDrmFormatModifierExplicitCreateInfoEXT`, as the spec allows, gives wrong
  pixels. The driver's size requirement leaves the offset out, and the kernel
  logged three NVRM assertions per import: "dmaAllocMapping_GM107: can't update
  VA space for mapping", "Assertion failed: vaHi <= pMemBlock->end @
  gpu_vaspace.c:2018" and "Check failed: NV_OK == status @
  virt_mem_allocator_gm107.c:2560".
- **The fix.** A zero plane offset with the memory bound at the pixmap's offset
  is equivalent per the spec and pixel-exact. It held for 256x256, 333x211,
  1024x768 and 1920x1080 at depth 24 and 300x200 at depth 32. The bind needs the
  offset aligned to the image's requirement (1024 bytes); every observed offset
  was a multiple of 4096. The kernel logged nothing after the change.
- **Import cost.** One import costs about 8 ms for the big shared buffer objects
  and 0.7 ms for the 8.8 MB one, apparently in proportion to what gets mapped. A
  cache per buffer object is impossible: re-exporting the same pixmap returns the
  same dma-buf inode, but every other pixmap in the same buffer object gets a new
  one. So each map or resize of a small window costs about 8 ms. Section 4.3
  proposes a way around it.

### 2.2 Synchronization with the X server's rendering

The X server's GPU rendering is asynchronous: the reply to a round trip means the
work was queued, not finished. After 300 full-window fills of a 2048x2048 window,
sampling right after the round trip was stale in 40 of 40 iterations, with three
sample points showing three different fills. With 20 fills the race did not show
in 5 iterations. It needs real GPU work in flight, which makes it rare in
practice and ugly when it hits: this is the tearing that picom's
`xrender-sync-fence` exists for on NVIDIA.

An X Sync fence fixes it. The sequence is CreateFence on the root once, then per
frame TriggerFence, AwaitFence, ResetFence and a round trip. That gave 0 of 40
stale samples, at the cost of waiting for the outstanding GPU work: 14.5 ms
after 300 fills, 0.8 ms after 20. The dma-buf never carried a pending implicit
fence (DMA_BUF_IOCTL_EXPORT_SYNC_FILE always returned a signalled sync file, even
mid-rendering), so kernel implicit sync cannot replace the X fence on NVIDIA.
DRI3 1.4 syncobjs are not available on this Xorg (DRI3 1.2), and they would not
apply anyway: they cover clients presenting through Present, not a compositor
reading other clients' pixmaps. The prototype fences before every frame that
samples a damaged DRI3 window, blocking the loop for as long as X's GPU work is
outstanding. Section 4.4 proposes removing the block.

### 2.3 Modifiers outside the NVIDIA X driver

On rootful Xwayland 24.1 (DRI3 1.4, glamor on NVIDIA GBM), window pixmaps export
with the compressed modifier 0x0300000000e08014. The NVIDIA Vulkan driver imports
only LINEAR and the uncompressed block-linear modifiers 0x0300000000606010 to
...15 for B8G8R8A8_UNORM. Plain pixmaps, such as the root background, cannot be
exported at all (BadPixmap). rxcom falls back per pixmap to MIT-SHM copies, and
the output stays exact. So whether the zero-copy route works depends on the
server's allocator, not on rxcom. That is the main argument for the X-side copy
route in 4.3.

### 2.4 Presentation and pacing

- **Xvfb.** NVIDIA presents to Xvfb through a CPU copy: about 32 ms per
  1920x1080 frame, with no vsync. That is fine for tests and meaningless for
  timing.
- **Xwayland.** FIFO paces correctly at 60 Hz. Frames go back to back until the
  three-image swapchain is full, then acquire blocks for about 16.5 ms. The
  queue means an animation can run up to two frames behind its input, which 4.2
  addresses.
- **CPU cost.** Recording and submitting a frame takes about 0.1 ms. A zoomed
  frame is two full-screen passes.
- **Real display.** Pacing there is untested. Nested servers cannot show NVIDIA
  Xorg's flips, its frame pacing or unredirection with games.

### 2.5 Test environment notes

- **lavapipe is broken by a partial upgrade.** vulkan-swrast 1:26.2.4-1, installed
  alone on 10-04, links libLLVM.so.23.1, while llvm-libs is 22.1.8-2 and mesa is
  26.2.2. The repositories have llvm-libs 23.1.3 and mesa/vulkan-swrast 26.2.5,
  so a full `paru -Syu` fixes it. Until then the nested tests run Vulkan on the
  NVIDIA driver, which presents to Xvfb through its CPU path.
- **Xvfb needs `-noreset`.** Otherwise it wipes atoms and root properties
  whenever the last client disconnects.
- **Focus follows the pointer without a window manager**, so xterm redraws its
  cursor whenever the pointer enters it. The tests pin focus to a window that
  ignores it.
- **`hsetroot` exits without retaining its pixmap**, and the next client received
  the same XID for its window. rxcom now ignores a root-pixmap XID that names a
  window.
- **The overlay window is not listed by QueryTree.**

### 2.6 The real display

The heavy synchronization probe in 2.2 ran on the user's live X server. It added
load during a five-minute freeze that, by Grizzco Lab's analysis, a Chrome popup
started. The two runs of the plane-offset variant in 2.1 produced the NVRM
assertions. Since then the rule is that nothing runs against the real display,
not even read-only queries, without the user's explicit agreement. All testing
uses nested servers. `examples/dri3_import.rs` keeps its synchronization probe
behind `--sync`, defaults to light settings, and says in its header that it must
not run against a desktop in use.

## 3. Parity with picom and xcompmgr

The user's picom setup today is glx with vsync, no shadows, no fading, and
`unredir-if-possible` with a 200 ms delay, plus the zoom patch. Rows marked
**(in use)** are the ones that setup depends on.

| Feature | xcompmgr | picom v13 | rxcom prototype | Proposed |
| --- | --- | --- | --- | --- |
| Composite redirected windows, ARGB alpha **(in use)** | yes | yes | yes, pixel-exact | done |
| GPU backend **(in use: glx)** | XRender only | xrender, glx, egl | Vulkan | Vulkan only |
| VSync **(in use)** | no | yes | FIFO swapchain | M2: paced frames, one frame of latency |
| Unredirect fullscreen windows **(in use, 200 ms)** | no | yes | no | M2 |
| Sync with X rendering on NVIDIA | no | `xrender-sync-fence`, opt-in | X Sync fence, always with DRI3 | M2: non-blocking |
| Damage-limited repaint | damage regions | buffer age | full repaint | M2: persistent scene, damaged regions only |
| `_NET_WM_WINDOW_OPACITY` | yes | yes | no (the shader has opacity) | M2 |
| Shaped (non-rectangular) windows | border clip | yes | draws the full pixmap | M2 |
| Desktop zoom **(in use)** | no | patched fork | yes | M2 polish, M4 per monitor |
| Shadows | yes | yes | no | M3, if wanted |
| Fading | yes | yes | no | M3, if wanted |
| Rounded corners | no | yes | no | M3, if wanted |
| Inactive dim and opacity | no | yes | no | M3, if wanted |
| Rules and config file | flags | rules language, libconfig | flags | M3: small config |
| Blur | no | yes | no | M4, if wanted |
| Multi-monitor awareness | no | yes (RandR) | one screen | M4 |
| Animations | no | scripts (v12+) | no | not planned |
| D-Bus | no | yes | no | not planned; X properties and a CLI instead |
| Custom window shaders | no | yes | no | not planned |

## 4. The full version: proposed architecture

### 4.1 Principles

- **One thread, one loop, no async runtime.** Everything rxcom waits for, the X
  connection, signals, timers and frame completion, fits in one `poll()`.
- **The GPU does all pixel work.** CPU copies exist only for servers without
  DRI3.
- **Every feature lands with a nested test** that compares pixels against an
  independent computation, as the prototype's do. Real-display trials happen only
  with the user's agreement.
- **Few dependencies.** x11rb, ash, libc, log and anyhow today. Add one only
  when a feature cannot reasonably do without it.
- **Vulkan only.** NVIDIA and Mesa drivers, with lavapipe for software rendering
  and tests.

### 4.2 Frame scheduling and latency

Rendering as soon as damage arrives wastes work on bursts and, with FIFO and
three images, lets frames queue up to two deep. The proposal is one frame in
flight, plus VK_KHR_present_id and VK_KHR_present_wait (NVIDIA has both) to
learn when each frame reached the screen. The next frame is then scheduled a
small margin before the following vblank. All damage arriving before that
moment merges into one repaint, and the pointer for the zoom is read at the last
moment. Where present_wait is missing, the X Present extension's NotifyMSC on the
root window gives the vblank clock instead, which is how picom paces.

### 4.3 Window contents: three routes

- **Zero-copy import, as now.** Used when the server exports a modifier the
  Vulkan driver can sample. On NVIDIA Xorg it costs about 8 ms per map or resize
  of a small window (2.1).
- **X-side GPU copy into buffers rxcom owns.** rxcom allocates exportable images
  with a modifier both sides accept: DRI3 GetSupportedModifiers intersected with
  Vulkan's list. It wraps them as X pixmaps with DRI3 PixmapFromBuffers. On
  damage it asks the server to CopyArea the damaged rectangles from the window
  pixmap into its own, followed by the fence.

  Each damage then costs a GPU blit of the damaged area inside the X server, and
  in return:
  - the import happens once per buffer, not once per resize (buffers can be
    over-allocated and reused);
  - compressed or unsupported modifiers stop mattering (2.3);
  - rxcom controls buffer lifetime.

  The open risk is whether the NVIDIA X driver accepts PixmapFromBuffers with its
  own block-linear modifier. That can be tested on the nested Xwayland first and
  then once on the real server, with permission. M2 would measure both routes
  and keep whichever is better per case.
- **CPU copies through MIT-SHM, as now.** For servers without DRI3 (Xvfb, Xvnc),
  improved to copy only damaged rectangles: DamageSubtract into a region, then
  XFixesFetchRegion. With VK_EXT_external_memory_host the server could write
  straight into Vulkan-visible memory.

### 4.4 Synchronization

X Sync fences stay, because they are proven on NVIDIA. They should stop
blocking the loop. rxcom would send TriggerFence and AwaitFence early in the
frame, do its CPU work, and wait for the round-trip reply only just before
submitting. It would fence only when DRI3-backed content was damaged, once per
frame. Nothing lets Vulkan wait on an X fence directly on NVIDIA, so the wait
remains on the CPU. It would be bounded, though, and overlapped with recording.

### 4.5 Partial repaint

The scene becomes a persistent image that rxcom owns. Each frame computes the
damaged screen region and redraws only that region of the scene, with scissor
rectangles. The region is the union of the windows' damage, the old and new
rectangles of windows that moved, resized, restacked, mapped or unmapped, and
background changes. A full-screen copy (unzoomed) or the zoom pass (zoomed) then
moves the scene into the swapchain image. Since the scene is ours, no buffer-age
tracking of swapchain images is needed. The extra copy costs tens of
microseconds on the 4070, far less than redrawing every window.

### 4.6 Unredirecting fullscreen windows

When the top-most mapped window covers the screen, is opaque (depth 24, no
shape, full opacity) and nothing is mapped above it, rxcom waits the configured
delay (picom's 200 ms in the user's config). It then calls
CompositeUnredirectWindow for that window, hides the overlay and stops
rendering, so the game scans out directly. It redirects again when another window
maps above, the window stops being fullscreen, or the user zooms in: zoom needs
compositing, so zooming into a game is allowed and re-redirects it.

### 4.7 Effects in one shader

The quad shader grows a few per-window parameters: opacity, corner radius and
dim.

- **Shadows** are a second quad beneath the window: an analytic rounded-rectangle
  signed-distance falloff for rectangular windows, so no textures are needed.
  Shaped windows use their bounding region as a mask.
- **Fading** animates opacity and keeps the last texture after unmap until the
  fade ends.
- **Blur** is dual-Kawase down- and up-sampling over the part of the scene behind
  a translucent window. It needs the persistent scene from 4.5, and the damage
  must grow by the blur radius.

### 4.8 Configuration and control

The configuration file would be small: rxcom's own TOML with picom's key names
where they exist (`vsync`, `unredir-if-possible-delay`, `shadow`, `fading`,
`corner-radius`), plus window rules matching on WM_CLASS, window type, name,
ARGB and fullscreen. Runtime control goes through root-window properties, as the
zoom already does, with a small `rxcomctl` command to set them. D-Bus only if a
real consumer appears.

### 4.9 Monitors

rxcom would read RandR monitors (GetMonitors) and follow RRScreenChangeNotify.
Zoom and unredirection then work per monitor: zoom applies to the monitor under
the pointer, with the mapping relative to that monitor's rectangle. One swapchain
on the overlay window can only pace to one refresh rate, so mixed refresh rates
stay a known limit, as they are for picom.

### 4.10 Zoom beyond the prototype

The protocol stays identical with the patched picom, and any change to the
mapping or easing lands in both. Candidates:

- **Event-driven pointer tracking** with XInput2 raw motion, instead of 125 Hz
  polling.
- **A filter option**: nearest or sharpened bicubic at high zoom, like macOS's
  "smooth images" toggle.
- **An optional magnified cursor.** rxcom would hide the hardware cursor with
  XFixesHideCursor and draw the XFixes cursor image scaled at the hotspot. Input
  still needs no transform.
- **Easing in log z**, so zoom speed feels uniform.
- **Anchoring at the pixel centre.** Today o = p·(1 − 1/z) anchors the top-left
  corner of the pixel under the hotspot. Anchoring its centre (p + 0.5) would
  centre the magnified pixel on the hotspot. Both compositors must agree, so this
  only changes in both at once.

### 4.11 Robustness

- **Device loss.** rxcom would rebuild the GPU side and keep the X state, or, if
  that fails, exit so the X server restores the screen.
- **Acquire with a timeout.** The prototype waits forever.
- **A watchdog thread.** It aborts the process when frames are due but the loop
  has made no progress for two seconds. The X server then undoes redirection on
  disconnect, which bounds any freeze rxcom could cause.
- **A tree-consistency check in debug builds**, comparing the window list with
  QueryTree.

### 4.12 Testing

The three tiers stay:

- **nested Xvfb** for exact pixels on the CPU route;
- **nested headless Xwayland** for DRI3 negotiation, modifiers and fallbacks;
- **the real display** only with the user's agreement, for pacing, flips and
  games.

The stress test grows with each feature. Once lavapipe works again, the tests can
also run without touching the GPU.

## 5. What each item left out of the prototype would take

| Item | Approach | Size | Notes |
| --- | --- | --- | --- |
| Shadows | analytic signed-distance shadows in the quad shader, a second draw per window; masks for shaped windows | ~250 lines, 1 to 2 days | damage must grow by the shadow extent |
| Fading | animate opacity, keep textures after unmap or destroy until faded | ~200 lines, 1 day | a fading window is damaged every frame |
| Blur | dual-Kawase passes over the scene behind translucent windows | ~350 lines, 2 to 3 days | needs the persistent scene (4.5) |
| Rounded corners | signed distance in the fragment shader | ~80 lines, half a day | corners are no longer opaque for occlusion |
| Rules and config | TOML (a parser dependency) or a hand-written subset; matching on class, name, type, ARGB, fullscreen | ~400 lines, 2 days | key names follow picom |
| D-Bus | zbus would bring a large dependency tree; X properties plus `rxcomctl` cover the known uses | ~100 lines without D-Bus | proposed: no D-Bus |
| Unredirect fullscreen | detection, delay, CompositeUnredirectWindow, hiding the overlay, re-redirect | ~250 lines, 1 to 2 days | needs a supervised real-display trial with a game |
| Multi-monitor | RandR monitor tracking, per-monitor zoom and unredirection | ~250 lines | mixed refresh rates stay unpaced |
| Non-Vulkan backends | a second renderer (XRender like xcompmgr, or GL) | ~800 lines each | not proposed; lavapipe covers machines without a Vulkan GPU |

## 6. Proposed milestones

**M2: daily driver on one monitor.** This milestone covers exactly what the
user's picom setup uses, done well:

- paced frames with at most one frame of latency (4.2);
- the persistent scene with partial repaint (4.5);
- the non-blocking fence (4.4);
- unredirect for fullscreen windows with the configurable delay (4.6);
- `_NET_WM_WINDOW_OPACITY`, and shaped windows;
- the robustness items (4.11);
- event-driven pointer tracking for the zoom;
- a measured decision between the zero-copy and X-side copy routes (4.3).

Acceptance: the nested tests extended to each item, then one supervised trial on
the real display (the user present) alongside the patched picom, switchable from
xmonad: no tearing, no stale frames, zero frames while idle, games unredirected.
Roughly 1,500 more lines.

**M3: optional effects, only those the user wants.** Rounded corners, shadows,
fading, inactive dim, the config file with rules, and `rxcomctl`.

**M4: monitors and blur.** RandR monitors, per-monitor zoom and unredirection,
and blur if wanted.

**M5: zoom extras.** The cursor magnification and filter options from 4.10, kept
in step with the picom patch.

## 7. Open questions for the user

1. Should rxcom aim to replace picom on the daily desktop, with M2 as the bar, or
   stay an experiment alongside the patched picom?
2. Which effects, if any, are wanted? The current picom config has shadows and
   fading off.
3. Config format: rxcom's own small TOML with picom's key names, or picom's
   libconfig syntax for drop-in compatibility?
4. Zoom: should the cursor be magnified? Is a smoothing toggle wanted at high
   zoom? Easing in log space? Each would change the picom patch too.
5. When M2 is ready, may it get a supervised trial window on the real display?
   Pacing, flips and unredirection with games cannot be tested nested.
6. A full `paru -Syu` would repair lavapipe (2.5) and let the tests run without
   the GPU. Is that a good time to do it?

## 8. M2: the X server composes, Vulkan presents (decided 10-29)

The goal for M2 is the user's: stop xcompmgr, start `rxcom &`, and use it as
the daily compositor on this machine. Two findings rule out the prototype's
design there.

- **Window contents.** Each DRI3 export of a window pixmap on NVIDIA's X driver
  exports the whole 600 to 700 MB heap buffer the pixmap lives in, and those
  buffers stayed allocated inside Xorg (2 GB of VRAM on 10-22, 2.1). An import
  also costs about 8 ms per map or resize.
- **Synchronization.** NVIDIA's X driver sometimes never wakes a SyncAwaitFence
  whose fence has completed: at VT switches for any client, and every one to
  two and a half minutes for picom on 10-28. picom then waits forever in the round trip
  after the await, which froze the desktop (`vt-debug2/repaint.md`). Only a
  TriggerFence from outside, or destroying the fence, releases it.

### 8.1 Measurements

`phase0`, a probe that acts as the compositor of a nested server (kept outside
the repository with its results), redirected four
windows on a 3840x2160 screen (one screen-sized, one half-screen, two ARGB at a
third of the screen) and made the X server redraw all of them every frame.
Medians:

| Per frame at 3840x2160 | Xvfb (software) | Xwayland, glamor on the 4070 |
| --- | ---: | ---: |
| ShmGetImage of the screen-sized window | 2.05 ms | 3.53 ms |
| ShmGetImage of an 800x600 rectangle | 0.11 ms | 1.33 ms |
| XRender of the four windows and the background into a scene pixmap | 4.40 ms | 0.03 ms, 0.25 ms of X CPU |
| XRender transform of the scene at 2x, nearest / bilinear | 3.3 / 6.3 ms | 0.02 / 0.02 ms |
| TriggerFence, then QueryFence until it reads triggered | first query | first query |

On a GPU-backed server a readback is the expensive operation: even a small
rectangle costs over a millisecond of the X server's single thread, because the
server must wait for its GPU and download. Composing the whole screen on the
server's GPU costs a fraction of that. glamor only flushes on TriggerFence, so
its fences say nothing about GPU completion; NVIDIA's X driver uses real GPU
fences, which is why 2.2 measured waits of up to 14.5 ms.

What the real display taught between 10-22 and 10-28, which no nested server can
reproduce:

- xcompmgr, which composes with XRender inside the X server and uses no fences,
  ran daily on this driver until 10-22, including VT round trips.
- Nothing suggests NVIDIA's X driver flips for the Present extension, and
  Present's copies tear (xserver issue 641). Its NotifyMSC does follow the OLED's
  vblanks, with a zero UST.
- GLX texture-from-pixmap, picom's route, waits for a vblank inside Xorg on every
  bind and release while any client has a vsync'd present pending: 6.6 to 7 ms per
  resize at 240 Hz, 14 to 24 ms at 60 Hz (`infra/xorg-resize-stall.md`). NVIDIA
  also loses every GL context at a VT switch.
- Vulkan on the NVIDIA xcb surface offers FIFO, FIFO_RELAXED, IMMEDIATE and
  FIFO_LATEST_READY, plus present_id and present_wait. FIFO presents and acquires
  block outright while no vblank comes (the OLED switched away, the screen
  blanked), and FIFO follows the lowest-numbered lit head, so the 60 Hz Philips
  mirror paces it at 60 Hz, as it did picom.

### 8.2 The choice

The X server composes the desktop; Vulkan only presents it.

- **Composition.** rxcom keeps one screen-sized scene pixmap. Each frame it asks
  the X server to composite the damaged part of the screen into it with XRender:
  the background, then every mapped window bottom to top through a picture on
  its named pixmap (borders included), with the clip set to the damage region.
  This is xcompmgr's paint path, the one proven on this driver. Window pixmaps
  never leave the X server: no per-window export, import or readback, and a map
  or resize costs two requests (name the pixmap, create a picture).
- **One shared buffer.** The scene is the only buffer Vulkan sees, imported once
  per screen size. rxcom allocates it in Vulkan with a modifier both sides accept
  and gives it to X with DRI3 PixmapFromBuffers. If the X server refuses, X
  allocates it and exports it once with BuffersFromPixmap (a screen-sized pixmap
  gets a buffer of its own, as the 1920x1080 one did in 2.1).
- **Presentation.** One full-screen pass draws the scene into the swapchain image,
  1:1 or zoomed, and presents it with FIFO. On the full-screen overlay window the
  driver flips at vblank, so nothing tears.
- **The X presenter.** Without the Vulkan route (no DRI3, as on Xvfb; an import
  the driver refuses; repeated device loss), the X server copies or zooms the
  scene onto the overlay window itself, with CopyArea or an XRender transform.
  That is xcompmgr's quality: it may tear, but it works on any server, and it is
  what the nested pixel tests check.
- **Zoom.** picom-zoom's rules, so both compositors agree: log-space easing with
  a 40 ms time constant, the centre of the pixel under the pointer fixed,
  picom's rounding of the zoomed view's origin to whole pixels, nearest
  filtering by default. Instead of picom-zoom's 4 ms poll, XInput 2 raw motion
  on the root tells rxcom that the pointer moved; it then queries the position
  at most once a frame, and does nothing while the pointer rests. A 100 ms poll
  remains for pointer warps, which make no raw motion. The prototype's easing
  and anchoring differed (section 1.5 and 4.10).

### 8.3 Synchronization

X's rendering into the scene and Vulkan's sampling of it still run on different
GPU channels, so they need a fence, but rxcom never sends SyncAwaitFence. After
its composition requests it sends TriggerFence and then QueryFence, a plain
query that reads the fence's state and does not depend on the wakeup NVIDIA
loses, repeating it every 0.2 ms until the fence reads triggered while the
Vulkan side records the frame. The X server never blocks on rxcom's behalf.
A fence that still reads untriggered after 100 ms is destroyed and replaced, the
frame goes out anyway, and the next frame repaints the whole scene, so a lost
fence costs at most one stale frame. rxcom also asks SYNC SetPriority for its
own connection, so the X server's scheduler serves its requests ahead of busy
clients.

In the other direction, X must not write the scene while Vulkan reads it. Before
composing, rxcom checks without blocking that the GPU has finished the previous
frame, through a timeline semaphore; that takes a fraction of a millisecond.

### 8.4 Presentation and pacing

- **FIFO_LATEST_READY.** A frame is composed and queued as soon as damage
  arrives, up to the frame limit (by default the fastest lit monitor's refresh
  rate, read from RandR and updated when the monitors change), and at each
  vblank the presentation engine shows the newest ready one. Acquire uses a
  zero timeout; when no image is free, damage merges into the next frame and
  rxcom looks again 1 ms later (20 ms after 50 ms without one). FIFO is the
  fallback where the mode is missing. present_wait is not needed for this and
  is not used.
- **Present fences** (swapchain_maintenance1) tell when the presentation engine
  is done with a present, so the swapchain is never destroyed while one is
  queued; the validation layers flagged exactly that without them.
- **No vblanks** (monitor off or away, DPMS, the VT switched away): presents stay
  queued and acquire finds no free image. rxcom keeps handling X events and
  shows the latest state once frames flow again; it never makes a call that
  waits for a vblank. On the nested Xwayland a 6 s stop of the frame clock cost
  rxcom 0.7% of a CPU.
- **VT switches.** rxcom watches `/sys/class/tty/tty0/active` and compares it
  with the root's `XFree86_VT`. When X's VT returns it sets the Vulkan side up
  again with a new scene and repaints everything.
- **RandR.** A new screen size gets a new scene and swapchain the same way.
- **GPU errors** rebuild the Vulkan side; after three without a quiet minute
  between them, the X server presents until the next rebuild.
- **A watchdog thread** aborts rxcom when a single turn of the loop has been stuck
  for 5 seconds outside poll(): a spin, or a driver call that never returns.
  Waiting for the X server does not count, since a stalled or grabbed server
  freezes the desktop whatever rxcom does. The X server undoes the redirection
  when rxcom's connection closes, so the desktop is usable meanwhile, and the
  watchdog starts a new rxcom, which backs off (1 s, then 2, 4, ... up to 32) and
  takes over once the stuck one is gone; after five restarts in a row it stops.
- **Signals.** SIGINT and SIGTERM restore the screen as the prototype does.
  SIGHUP is ignored, so `rxcom &` outlives the terminal it was started from.
- **Unredirection,** picom's rule with its 200 ms delay: when the top-most mapped
  window covers the screen and is opaque, and the zoom is off, rxcom waits for
  its GPU work to finish (bounded), destroys the swapchain, unredirects the
  windows and unmaps the overlay, as picom does. It redirects again as soon as
  that stops being true, or when the zoom starts.

### 8.5 Costs at 4K and 240 Hz

- **X server:** the composition requests, about 0.25 ms of CPU per frame for four
  windows redrawn in full on glamor, less when only part of the screen changes,
  and nothing when idle. No readbacks and no GLX binds.
- **rxcom:** about 0.1 ms of CPU per frame, measured over 670 zoomed 4K frames on
  the nested Xwayland (release build), fence queries included.
- **GPU:** X's composition of the damaged area, plus one full-screen pass per
  frame (about 67 MB of memory traffic at 4K, roughly 0.13 ms on the 4070).
- **Memory:** the scene (33 MB) and four swapchain images (133 MB) in video
  memory; 95 to 115 MB resident, nearly all of it NVIDIA's driver (4 to 5 MB
  with the X presenter).
- **Idle:** no frames and no CPU; while zoomed and still, a pointer query every
  100 ms.

### 8.6 Rejected

- **Per-window DRI3 import**, the prototype's route: the heap buffers pinned in
  Xorg and 8 ms per map or resize (2.1).
- **Per-window MIT-SHM copies:** each readback blocks the X server's only thread
  for its GPU wait and download, 1.3 to 3.5 ms each on a GPU-backed server (8.1).
  A few damaged windows at 240 Hz would need more than the whole thread.
- **GLX texture-from-pixmap:** the vblank wait inside Xorg on every bind, GL
  contexts lost at every VT switch, and it needs the same fence.
- **XRender composition presented by the X server alone** (xcompmgr's whole
  design): the least code and no GPU context in rxcom, but it tears on this
  driver. It stays as the fallback presenter.
- **Waiting on X Sync fences inside the X server** (SyncAwaitFence): the lost
  wakeups. DRI3 1.4 syncobjs are not in Xorg 21, and NVIDIA attaches no implicit
  fences to dma-bufs (2.2).
- **MAILBOX** does not exist on this surface; **IMMEDIATE** tears.

### 8.7 What only the real display can answer

- Whether NVIDIA's X driver accepts PixmapFromBuffers and renders into such a
  pixmap at full speed; otherwise the BuffersFromPixmap route, otherwise the X
  presenter. rxcom logs the route it took, and the time each composition into
  the shared scene takes until its fence reads triggered, so "full speed" gets
  a number.
- On the BuffersFromPixmap route, whether the exported scene's buffer stays
  allocated inside Xorg after rxcom exits, as the 10-22 exports did: Xorg's
  memory in `nvidia-smi` before and after two or three restarts of rxcom.
- Whether QueryFence keeps reading triggered where SyncAwaitFence lost its
  wakeup, and how long the polled waits are at 240 Hz.
- Flips, pacing at 240 Hz, VT round trips, the monitor going away, and games
  unredirected.

`rxcom -v` logs the route, the fence waits and per-frame timings, so the user's
first trial answers these.

### 8.8 As built (10-29)

| File | Lines | Role |
| --- | ---: | --- |
| `src/main.rs` | 136 | options, signals, start and restart |
| `src/x.rs` | 390 | connection, extensions, selection, overlay, DRI3, fence, helpers |
| `src/windows.rs` | 130 | the stacking list and each window's X resources |
| `src/scene.rs` | 192 | the XRender composition, the X presenter, the background |
| `src/compositor.rs` | 638 | events, damage, frames, unredirection, VT, recovery, the loop |
| `src/gpu.rs` | 771 | the Vulkan presenter: the shared scene, the pass, the swapchain |
| `src/zoom.rs` | 115 | picom-zoom's easing and view |
| `src/watchdog.rs`, `vt.rs`, `logger.rs`, `lib.rs` | 184 | restarts, the VT watch, logging |
| `shaders/` | 28 | the full-screen pass |

That is about 2,550 lines of Rust without its unit tests, against xcompmgr's
roughly 2,500 lines of C, which have shadows and fading but no Vulkan, zoom,
unredirection or recovery. The dependencies are x11rb, ash, libc, log and
anyhow; glslc builds the shaders.

Tested on nested servers only (section 2.5): `tools/nested-test.sh` on Xvfb
(the X presenter) and on headless Xwayland with the NVIDIA driver and the
validation layers (the Vulkan presenter), `tools/stress-test.sh` on both, plus
a stopped frame clock, two RandR resizes and the watchdog's restart by hand.
On the nested Xwayland the X server took the Vulkan image through
PixmapFromBuffers with modifier 0x0300000000606014, the one NVIDIA's X driver
itself uses.

Not used, though NVIDIA 615 offers them: present_id2/present_wait2 and their
first versions (FIFO_LATEST_READY with a zero-timeout acquire paces without
them), and VK_EXT_present_timing (not in ash's headers, and nothing needs vblank
timestamps yet).
