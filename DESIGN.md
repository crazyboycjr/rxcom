# rxcom design

rxcom is an X11 compositor written in Rust that renders with Vulkan. The long-term
goal is a compositor that can replace picom on the daily desktop, with the
mainstream features of picom and xcompmgr plus a macOS-style desktop zoom. This
document describes the prototype as it stands, what building it taught us, and a
proposal for the full version. Nothing beyond the prototype gets built until the
user has agreed to the proposal (sections 4 to 7).

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
