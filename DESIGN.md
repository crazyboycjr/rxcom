# rxcom design

rxcom is an X11 compositor written in Rust. It composites the windows of an X
desktop without effects or a configuration file, gives the screen to
full-screen games, and magnifies the whole desktop around the pointer when a
root window property asks it to. This document describes how rxcom 0.3.0
works, why it is built this way, and what was measured and learned on the way,
much of it about NVIDIA's X driver.

In short: the X server composes the desktop itself, with XRender, into one
screen-sized pixmap, and a presenter puts that pixmap on the screen. The
default presenter is the X server too, which copies the pixmap onto the
composite overlay window, and it is the supported one. The GL and Vulkan
presenters flip the pixmap at vblank instead; they are experimental, for the
reasons in sections 6 and 7.

## 1. Goals

- Composite an X desktop for what compositing is needed for: translucent
  windows blended correctly, no tearing where the driver allows it, and games
  given the screen to themselves. No shadows, fading, blur, rounded corners,
  animations or configuration file; a few command-line options.
- Zoom the whole desktop around the pointer, as macOS does, driven by a window
  manager or a script through one root window property, with the same mapping
  as picom-zoom, a patched picom, so that both agree pixel for pixel.
- Stay small and quick: one thread and one poll() loop, no async runtime, few
  dependencies, and no CPU use while nothing on the screen changes.
- Work on NVIDIA's proprietary X driver, where most of the findings below come
  from.

## 2. Architecture

```
X events --> window list, damage --> composition with XRender, in the X server
                                                   |
                                                   v
                                   the scene: one screen-sized pixmap
                                                   |
                     +-----------------------------+-----------------------------+
                     |                             |                             |
                     v                             v                             v
           X presenter (default)          GL presenter                  Vulkan presenter
         XRender onto the overlay    texture from pixmap,         a DRI3-shared image,
          window, in the X server       flip at vblank                flip at vblank
```

One thread runs one loop. Each turn reads the signalfd, drains X events into
the window list, advances the zoom, follows the pointer while zoomed, and draws
a frame when one is due. Otherwise it sleeps in ppoll() on the X connection,
the signalfd and the kernel's active-VT file, with a timeout to the microsecond
for whatever comes next. The only other thread is the watchdog's (section 12).

rxcom takes the `_NET_WM_CM_Sn` selection and refuses to start while another
compositor holds it. It redirects the root's children manually and draws into
the composite overlay window, whose input shape is empty, so every click falls
through to the windows below. It tracks the top-level windows from a QueryTree
taken under a server grab, and afterwards from CreateNotify, DestroyNotify,
MapNotify, UnmapNotify, ConfigureNotify (geometry, and stacking through
`above_sibling`), ReparentNotify, CirculateNotify and GravityNotify. If another
compositor takes the selection, rxcom exits without touching the redirection or
the overlay, which now belong to the newcomer.

On SIGINT or SIGTERM rxcom frees what it made, unredirects, releases the
overlay and the selection, and leaves the screen as it found it. SIGHUP is
ignored, so `rxcom &` outlives the terminal that started it.

## 3. Composition in the X server

rxcom keeps one screen-sized pixmap, the scene. Each frame it asks the X server
to redraw the damaged part of the screen into it with XRender: the background
first, then every mapped window from the bottom of the stack to the top, each
through a picture on its named pixmap (borders included), with the clip of the
scene's picture set to the damage region. Opaque windows are copied (Src),
windows of depth 32 are blended as premultiplied alpha (Over), and shaped
windows are clipped to their bounding shape. This is xcompmgr's paint path.

Each window has a Damage object at the NonEmpty level. A damage event moves
the window's damage into the scene's damage region (DamageSubtract into a
region, translated to where the window is), and moves, resizes, restacks, maps
and unmaps add the window's old and new rectangles. The background is the root
pixmap that `_XROOTPMAP_ID` or `ESETROOT_PMAP_ID` names, tiled, or a solid
colour. Some wallpaper setters exit without keeping their pixmap, and the X
server may then give its XID to a new window, so a root pixmap ID that names a
window is ignored.

Window contents never leave the X server: there is no per-window export,
import, upload or readback, and mapping or resizing a window costs two
requests, NameWindowPixmap and CreatePicture. Section 4 explains why this
matters.

## 4. Why the X server composes

rxcom's first prototype composed in Vulkan, with each window's pixmap shared
through DRI3, or copied through MIT-SHM where it could not be shared. It was
pixel-exact on nested servers, and on NVIDIA's X driver it ran into the three
problems below. The X server composing is what xcompmgr has always done, and
xcompmgr, which uses no fences either, runs on NVIDIA's driver through VT
switches without trouble.

### 4.1 DRI3 exports of window pixmaps

These measurements come from a component test on Xorg 21.1 with NVIDIA's 610
driver and an RTX 4070 SUPER, using an off-screen window that the test
redirected itself.

- **Every export pins a large buffer.** BuffersFromPixmap returns one plane
  with modifier 0x0300000000606014 (block-linear, uncompressed) for depth 24
  and depth 32 alike. Small pixmaps are suballocated: each export's dma-buf
  names the whole buffer object, 640 to 950 MB, and the pixmap sits at an
  offset inside it (262144, 991232, 55126016 and so on). Exported buffers
  stayed allocated inside Xorg until it restarted, 2 GB of video memory by
  the end of the tests. A 1920x1080 pixmap had an 8.8 MB buffer of its own.
- **The plane offset.** Putting the pixmap's offset into the plane layout of
  `VkImageDrmFormatModifierExplicitCreateInfoEXT`, as the specification allows,
  gives wrong pixels, and the kernel logs three NVRM assertions per import:
  "dmaAllocMapping_GM107: can't update VA space for mapping", "Assertion
  failed: vaHi <= pMemBlock->end @ gpu_vaspace.c:2018" and "Check failed:
  NV_OK == status @ virt_mem_allocator_gm107.c:2560". A zero plane offset with
  the memory bound at the pixmap's offset is equivalent by the specification,
  pixel-exact and quiet. The bind needs the offset aligned to the image's
  requirement (1024 bytes), and every offset observed was a multiple of 4096.
  The Vulkan presenter still imports this way when it falls back to an X
  pixmap (6.1).
- **Imports are slow.** One import costs about 8 ms for the big shared buffer
  objects and 0.7 ms for the 8.8 MB one, apparently in proportion to what gets
  mapped, so every map or resize of a small window would cost about 8 ms. A
  cache per buffer object is impossible: exporting the same pixmap again
  returns the same dma-buf inode, but every other pixmap in the same buffer
  object gets a new one.

Other servers have other limits. On a rootful Xwayland 24.1 (DRI3 1.4, glamor
on NVIDIA's GBM), window pixmaps export with the compressed modifier
0x0300000000e08014, which NVIDIA's Vulkan driver cannot import (it takes LINEAR
and the uncompressed block-linear modifiers 0x0300000000606010 to
0x0300000000606015 for B8G8R8A8_UNORM), and plain pixmaps such as the root
background cannot be exported at all (BadPixmap). Whether zero-copy works
depends on the server's allocator, not on the compositor.

### 4.2 Readbacks

On a GPU-backed X server, reading a window back is the expensive operation:
even a small rectangle costs more than a millisecond of the X server's only
thread, which has to wait for its GPU and download. A probe that acted as the
compositor of a nested server redirected four windows on a 3840x2160 screen
(one screen-sized, one half-screen, and two ARGB windows a third of the
screen's size) and made the X server redraw all of them every frame. The
medians per frame:

| Per frame at 3840x2160 | Xvfb (software) | Xwayland, glamor on an RTX 4070 SUPER |
| --- | ---: | ---: |
| ShmGetImage of the screen-sized window | 2.05 ms | 3.53 ms |
| ShmGetImage of an 800x600 rectangle | 0.11 ms | 1.33 ms |
| XRender of the four windows and the background into a scene pixmap | 4.40 ms | 0.03 ms, and 0.25 ms of the X server's CPU |
| XRender transform of the scene at 2x, nearest / bilinear | 3.3 / 6.3 ms | 0.02 / 0.02 ms |

A few damaged windows at 240 Hz would need more than the whole X server thread
for their readbacks, while composing the whole screen on the server's GPU costs
a fraction of a millisecond.

### 4.3 GLX texture-from-pixmap per window

picom's GLX backend binds every window's pixmap as a texture. On NVIDIA's X
driver, glXCreatePixmap, glXBindTexImageEXT, glXReleaseTexImageEXT and
glXDestroyPixmap wait for a vblank inside Xorg whenever any client has a
vsync'd present pending: 6.6 to 7 ms per window resize at 240 Hz, and 14 to
24 ms at 60 Hz. Binding per window would also need the fence of section 5 for
every frame. The GL presenter binds one pixmap, the scene, once (7.2).

## 5. Synchronization

### 5.1 The X server's rendering is asynchronous

The reply to a round trip means that the X server has queued the work, not
that the GPU has done it. After 300 full-window fills of a 2048x2048 window on
NVIDIA's X driver, a sample taken right after the round trip was stale in 40
tries out of 40, with three sample points showing three different fills; after
20 fills the race did not show in five tries. It needs real GPU work in
flight, which makes it rare in practice and ugly when it hits: it is the
tearing that picom's `xrender-sync-fence` option exists for on NVIDIA.

An X Sync fence fixes it: TriggerFence after the rendering, then a wait until
the fence is triggered. That gave no stale samples in 40 tries, at the cost of
waiting for the outstanding GPU work, 14.5 ms after 300 fills and 0.8 ms after
20. Nothing lighter replaces it on NVIDIA. Its dma-bufs carry no implicit fence
(DMA_BUF_IOCTL_EXPORT_SYNC_FILE always returned a signalled sync file, even
mid-rendering), and DRI3 1.4's syncobjs are not in Xorg 21 (it has DRI3 1.2);
they would not apply anyway, since they cover clients presenting through the
Present extension, not a compositor reading other clients' pixmaps. glamor, for
its part, only flushes at TriggerFence, so on glamor a triggered fence says
nothing about the GPU being done.

### 5.2 Lost wakeups

NVIDIA's X driver sometimes never wakes a SyncAwaitFence whose fence has
completed: at VT switches, for any client, and, under picom, every one to two
and a half minutes. A compositor that waits in the round trip after its
SyncAwaitFence then waits forever, and the desktop freezes; only a TriggerFence
from another client, or destroying the fence, releases it.

So rxcom never sends SyncAwaitFence. After its composition requests it sends
TriggerFence and then QueryFence, a plain query that reads the fence's state
and does not depend on the wakeup NVIDIA loses, every 0.2 ms until the fence
reads triggered. A fence still untriggered after 100 ms is destroyed and
replaced, the frame goes out anyway, and the next frame repaints the whole
scene, so a lost fence costs at most one stale frame. rxcom also asks SYNC
SetPriority for its own connection, so that the X server's scheduler serves its
requests ahead of busy clients.

The X presenter needs no fence at all, since the composition and the copy onto
the overlay both run in the X server, in order. Only the presenters that read
the scene on a GPU channel of their own wait for it: Vulkan, and GL where the
driver cannot wait on the GPU (7.3).

### 5.3 The other direction

The X server must not draw into the scene while the GPU still reads it for an
earlier frame. Before it composes, rxcom checks without blocking that the last
frame that read the scene is done: through a timeline semaphore with Vulkan,
and through a GL fence with GL. A frame that presents an image again without
drawing it (6.3) reads nothing and does not count.

## 6. The Vulkan presenter (experimental)

### 6.1 Sharing the scene

The scene is the only buffer Vulkan sees, and it is shared once per screen
size. rxcom allocates it in Vulkan with a DRM modifier both sides accept
(Vulkan's exportable, single-plane modifiers for sampling B8G8R8A8_UNORM,
intersected with what DRI3 GetSupportedModifiers lists) and gives it to the X
server with DRI3 PixmapFromBuffers. NVIDIA's X driver accepts it: on an RTX
4070 SUPER at 3840x2160 it took the image with modifier 0x0300000000606014, the
one it uses itself, and composed into it at full speed. With a window
animating, a composition took 0.6 to 0.8 ms on average until its fence read
triggered, and 1.4 ms at most. Where the X server refuses, it allocates the
pixmap itself and exports it once with BuffersFromPixmap, bound with the offset
of 4.1; a screen-sized pixmap gets a buffer of its own. Without DRI3 1.2 (Xvfb,
for one), or where Vulkan cannot start, the X server presents.

### 6.2 The pass

One full-screen pass samples the scene, 1:1 or zoomed, into a swapchain image
on the overlay window. Vulkan presents through a second libxcb connection, so
the driver's presentation traffic never interleaves with rxcom's events. The
scene passes to and from the X server with queue-family-foreign barriers where
VK_EXT_queue_family_foreign exists. The swapchain covers the screen, so the
driver flips it at vblank and nothing tears.

### 6.3 Presenting

- **FIFO_LATEST_READY.** A frame is composed and queued as soon as damage
  arrives, up to the frame limit, and at each vblank the presentation engine
  shows the newest frame that is ready. Acquire uses a zero timeout: when no
  image is free, the damage merges into the next frame, and rxcom looks again
  1 ms later, 20 ms later once 50 ms have gone by without an image, and every
  250 ms while it has nothing new to show. FIFO is the fallback where the mode
  is missing. MAILBOX does not exist on NVIDIA's xcb surface, IMMEDIATE tears,
  and present_wait is not needed.
- **Present fences** (VK_EXT_swapchain_maintenance1) tell when the presentation
  engine is done with a present, so the swapchain is never destroyed while one
  is queued. The validation layers flagged exactly that without them.
- **Images that hold the frame already.** Presenting only reads an image, so
  each swapchain image remembers which version of the screen it holds, and an
  image that holds the current version is presented again without drawing: an
  empty submit passes its acquire semaphore on to the present. After a change,
  each image is drawn once more.
- **No vblanks.** rxcom never makes a call that waits for a vblank. With the
  monitor off or away, the screen blanked, or another VT in front, presents
  stay queued and acquire finds no free image; rxcom keeps handling X events
  and shows the latest state once frames flow again. On a nested Xwayland a
  6 s stop of the frame clock cost rxcom 0.7% of a CPU.

### 6.4 Variable refresh, and presenting at every refresh

A full-screen swapchain that flips puts a G-SYNC or G-SYNC Compatible
(FreeSync) monitor into variable refresh, and NVIDIA's driver gives Vulkan
programs on X no way to opt out. Its application profiles and
`__GL_VRR_ALLOWED` are OpenGL's, it folds `__GL_GSYNC_ALLOWED` into the same
setting, its VKDirect keys apply only to direct-to-display swapchains, and the
X11 WSI sets no window property and passes no flag with its presents. With
NVIDIA's VRR indicator on, the indicator read "G-SYNC" whichever of these was
set (driver 615). Presenting only on damage then let the refresh rate follow
the desktop down to the monitor's floor (48 Hz) whenever it rested, so the
cursor, which moves only at a refresh, felt like 60 Hz, and an OLED monitor
blacked out for seconds each time variable refresh engaged.

So by default the Vulkan presenter presents at every refresh of the fastest lit
monitor: it queues a present every 95% of a refresh period, so that a
variable-refresh monitor never waits for a frame and stays at its top rate, and
FIFO_LATEST_READY shows the newest frame at each vblank. Such a present never
delays a frame with news, which supersedes it, and costs the X server nothing:
no composition and no fence. It needs a present mode in which a present
replaces a queued one (FIFO_LATEST_READY or IMMEDIATE); with FIFO it would keep
the queue full and add frames of latency, so there rxcom presents only on
changes. Nothing is presented while a window has the screen to itself, while
the screen saver blanks the screen, while no monitor is lit, or while another
VT is in front. `--no-continuous` presents only on changes.

The price is power: presenting at every refresh holds the GPU at its top memory
clock (section 13). Turning variable refresh off for the whole screen (NVIDIA's
AllowVRR, the AllowGSYNCCompatible MetaMode, or the monitor's menu) would avoid
it, but would take variable refresh from games too, and a compositor gains
nothing from it. This is why the Vulkan presenter is experimental.

### 6.5 Rebuilding

When the X server's VT comes back to the front, rxcom sets the Vulkan side up
again with a new scene and repaints everything, and it does the same for a new
screen size. A GPU error rebuilds it too; after three GPU errors with less than
a minute between them, the X server presents for the rest of the run.

## 7. The GL presenter (experimental)

GL can flip without tearing, like Vulkan, and NVIDIA's OpenGL honours
`__GL_VRR_ALLOWED=0`, so GL can present only what changed and leave the GPU
idle otherwise. rxcom sets `__GL_VRR_ALLOWED=0` for itself unless it is set
already. The X server still composes the scene; GL only shows it.

### 7.1 Set-up

- **GLX, not EGL.** picom's EGL backend once hit an NVIDIA Xid 31 MMU fault,
  after which every GL call returned GL_CONTEXT_LOST; its GLX backend did
  not.
- **Loaded only when chosen.** libGL and libX11 are loaded at run time
  (x11-dl), with an Xlib connection of GL's own, so the other presenters never
  load them, and `__GL_VRR_ALLOWED` is set before the driver reads it.
- **Robust, without purge reports.** The context is robust where GLX offers
  it, so a GPU reset reads as a lost context and rxcom sets GL up again, but it
  never opts into GLX_NV_robustness_video_memory_purge. That opt-in is what
  makes NVIDIA report a reset at every VT switch (Chromium's GPU process dies
  then); without it the driver keeps the context's memory, and a return to the
  X server's VT only repaints.
- **Only while vblanks arrive.** GL is set up, and moved to a new scene after
  a RandR change, only while vblanks arrive, since NVIDIA's pixmap calls wait
  for one (4.3) and with the monitor off none comes. The X server presents
  until then. Without vblanks, ending GL leaves its binding to the process's
  exit rather than unbinding it.

### 7.2 One texture, bound once

GLX_EXT_texture_from_pixmap binds the scene pixmap when the scene is made.
Whether a binding shows later drawing without a rebind is undefined by the
extension, so rxcom tests it at start: the X server fills the scene, GL samples
it into one pixel of a back buffer that is never shown, the X server fills it
again, and GL samples again. Where the second sample is stale (Mesa's software
rasteriser copies the pixmap when binding it), every frame rebinds. NVIDIA's
binding shares the pixmap's memory, and it is never rebound.

### 7.3 Waiting for the composition on the GPU

rxcom imports four X fences once with GL_EXT_x11_sync_object and uses them in
turn, as KWin's X11 compositor does: it triggers one after its composition
requests, and the GPU waits for it (glWaitSync) before sampling the scene. Once
the fence reads signalled, rxcom resets it, and collects a round trip begun
with the reset before using the fence again, since a GPU wait issued before the
X server processed the reset could see the old trigger. The extension leaves
resets undefined, so rxcom tests at start that a wait issued after a trigger
ends, and again after a reset; where one does not, GL is refused. Mesa has no
GL_EXT_x11_sync_object, and there rxcom polls the X fence as in 5.2.

The trigger has to come first. NVIDIA's glWaitSync on an imported X fence does
not return until the X server has triggered the fence, and polls on the CPU
inside the driver until then, so a wait issued before its trigger hangs the
calling thread. At a VT switch the X server triggers nothing, and NVIDIA's X
server holds the switch until the client stuck in glWaitSync is gone. In one
such hang the watchdog killed rxcom five seconds later, and the X server came
back without most of its keyboard and mouse devices, whose logind pauses it had
answered too late; with systemd-logind 258 or newer and a rootless X started by
startx, they may not come back at all
([systemd issue 43352](https://github.com/systemd/systemd/issues/43352)).

So rxcom polls the fence's status itself (glGetSynciv), outside the driver, and
calls glWaitSync only once the fence reads triggered. A frame whose trigger has
not come within 50 ms gives up, and the next frame redraws everything; a fence
not triggered after 100 ms is triggered again. This keeps rxcom out of the
driver's wait, but the X server's VT switch can still stall with the GL
presenter on NVIDIA, which is why GL is experimental.

### 7.4 Only what changed, and nothing blocks

- **Partial redraws.** GL swaps with interval 1 on the overlay window, draws a
  frame only when the scene or the view changed, and uses GLX_EXT_buffer_age to
  redraw only what the back buffer lacks: the new damage and that of every
  frame since that buffer was last shown (four frames are remembered). The
  damage is kept as a box around whole windows, since a NonEmpty damage event
  tells only where a window's first drawing went. A zoomed view, a new view or
  an unknown age redraws everything.
- **No waits.** At most two swaps are in flight, judged by GL fences checked
  without waiting; the X server composes into the scene only once a fence
  after the last draw says GL has read it; and no frame is drawn while no
  vblanks arrive (no lit monitor, the screen saver on, another VT in front),
  since the swap would wait for one. With `-v`, the slowest swap call and the
  slowest wait for a composition's fence of every ten seconds are logged.
- **The same pixels.** The zoom maps pixels as the Vulkan shader does, with
  GL's window and texture rows running upward handled in the shader, so the
  nested suite's exact checks hold.

### 7.5 What it measured

On an RTX 4070 SUPER at 3840x2160 and 240 Hz, with ForceFullCompositionPipeline
off and a program redrawing about 230 times a second: the binding shares the
scene, so it is never rebound; the fences release, the slowest glWaitSync of
each ten seconds taking 0.33 to 0.6 ms; almost every frame is a partial redraw;
the slowest swap call takes 0.16 to 0.49 ms; NVIDIA's VRR indicator stays off
and nothing tears. Under that load the GPU drew 42 W at P3, against 72 W at P0
for the X presenter with ForceFullCompositionPipeline (section 13).

## 8. Windows their clients have not drawn yet

When the X server maps or resizes a redirected window, it fills the window's
new pixmap with a copy of the screen pixmap there (`compNewPixmap`). Under the X
presenter that is the last composed frame, so such a window looks like what was
on the screen before. When the GL or Vulkan swapchain flips (NVIDIA's
composition pipeline off), nothing updates the screen pixmap: it keeps the
frame the X server last drew there, such as the desktop when rxcom started, or
black after a VT switch, and a new window flashes it until its client draws:
old text, another workspace's windows, or black. A readback confirmed it: new
window pixmaps copy whatever the screen pixmap holds, and with the composition
pipeline on, flipped frames do reach the screen pixmap.

So until its client has drawn, rxcom draws a window from a stand-in: a copy of
the scene taken where the window lies when its new pixmap is bound, which is
what the X presenter's screen would show. The X server's own damage for a new
pixmap says nothing about the client: it reports the whole window when mapping
it (`compSetRedirectBorderClip`) and repaints the border, reported as its
bounding box. rxcom selects Expose on top-level windows too, and the server
sends it before the client can draw, so damage after it comes from the
client's requests. NonEmpty damage can merge the client's first drawing into
the server's report, so a second damage object reports each drawing request's
rectangles in order (RawRectangles) while a window waits. Rectangles within the
window after the Expose are the client's, and the wait ends when they cover
the window: at once for a GL swap or a whole image, later for a drawing in
parts. A drawing in parts may be the rest of one begun before the window was
mapped again, so its first part proves nothing, and it gets 500 ms at most.

Such reports cost too much to keep for every window: a terminal printing `yes`
made 67,000 to 80,000 of them a second and tripled the X server's CPU. So the
second damage object lives only from a window's unmap until its client has
drawn after the next map. Where rxcom cannot watch from before the server's
damage, it asks the client to draw again with ClearArea and exposures: for a
window mapped as it was created (a menu), and for a resized window whose client
has not drawn within 50 ms, since the client may have redrawn before rxcom
heard of the resize; a slow client's pending drawing absorbs the request. A
window whose top level gets no Expose (a child covers it) shows its own pixels
after 50 ms.

## 9. Zoom

### 9.1 The property

The window manager, or any other client, writes the target magnification to
the root window property `_SCREEN_ZOOM`: CARDINAL, format 32, one value in
16.16 fixed point. No property, or a value of 65536 or less, means no zoom, and
values above 16x are clamped to 16x. picom-zoom follows the same rules under
the name `_PICOM_ZOOM`. A level set before rxcom starts shows at once, without
easing.

### 9.2 Easing and mapping

The level eases exponentially toward the target in log space, with a time
constant of 40 ms, and snaps to the target once within 0.1%, about 0.3 s for a
step from 1x to 3x. The easing clock starts when the target changes, so a long
idle period never produces a jump.

The mapping is picom-zoom's, so both compositors draw the same pixels. Per
axis, the view keeps the centre of the pixel under the pointer in place: with
the pointer's centre p (the pointer's pixel plus a half) and level z, the view
starts at p·(1 − 1/z). rxcom takes the part of the screen visible at that level
with a pixel of margin (the crop), rounds the crop's position on the screen
(the origin) to whole pixels, and clamps it so that every screen pixel samples
at least half a pixel inside the crop. Screen pixel s then shows the desktop at
`crop + (s + 0.5 − origin) / z`, in pixel-centre coordinates. The desktop point
under the hardware cursor stays where it is, so input needs no transform, and
the cursor is left alone, at its normal size.

Nearest filtering is the default, and keeps every pixel square: the nested
tests check it pixel for pixel against the mapping computed independently in
Python, and unzooming restores the exact image. `--zoom-filter linear` samples
bilinearly instead.

### 9.3 Following the pointer

While zoomed, XInput 2 raw motion on the root tells rxcom that the pointer
moved, and rxcom then queries the position at most once a frame; while the
pointer rests it does nothing. A poll every 100 ms catches pointer warps, which
make no raw motion. Without XInput 2, rxcom polls every 4 ms, as picom-zoom
does.

### 9.4 Driving it

Anything that can set a root property can zoom: `xprop`, a script bound to keys,
or the window manager itself (the README has examples). Wheel bindings need
care: X delivers a wheel notch to programs that use XInput 2 smooth scrolling as
scroll motion before the emulated button press that would trigger a passive
grab, so with a plain grab of a modifier and the wheel, the window under the
pointer scrolls as well. A window manager that holds the pointer from the
modifier's press, and replays what is not a wheel event, avoids it.

## 10. Frames and pacing

- **The frame limit** is the refresh rate of the fastest lit monitor, from
  RandR's CRTC modes (the dot clock over the total pixels per frame, with
  doublescan and interlace accounted for), updated when the CRTCs change, or
  `--max-fps`. Without RandR it is 60 Hz.
- **Damage merges.** A frame is composed as soon as damage arrives, at most at
  the frame limit, and damage that arrives meanwhile merges into the next
  frame. The X presenter copies only the damaged part of the scene unless the
  view is zoomed or the overlay lost its contents.
- **Sleeping to the microsecond.** rxcom sleeps in ppoll() with a timespec:
  poll()'s whole milliseconds had spaced timed frames up to 5 ms apart.
- **When vblanks come.** A monitor must be lit (RandR), the screen saver off
  (MIT-SCREEN-SAVER), and the X server's VT in front. rxcom watches
  `/sys/class/tty/tty0/active`, which the kernel notifies at every switch, and
  compares it with the root's `XFree86_VT`, since the X server sends no event
  for it. GL draws nothing while no vblanks come, and Vulkan stops presenting
  at every refresh.

## 11. Unredirection

rxcom follows picom's rule, with its 200 ms delay: when the top-most mapped
window covers the screen and is opaque (depth 24, no shape), and the zoom is
off, rxcom waits for its GPU work to finish (for a bounded time), destroys the
swapchain, unredirects the windows and unmaps the overlay, so a game flips to
the display by itself and keeps its own variable refresh. It redirects again as
soon as that stops being true, or when the zoom starts: zooming into a game is
allowed and composites it again.

`_NET_WM_BYPASS_COMPOSITOR` works as KWin and Mutter read it: 1 gives the
screen to a covering window even when it has alpha, and 2 keeps a window
composited (a lock screen that must not flip into variable refresh, for one).
rxcom reads it on the root's children, so under a reparenting window manager
it takes effect only where the frame window carries it.

## 12. Robustness

- **The watchdog.** A thread aborts rxcom when a single turn of the loop has
  been stuck for 5 seconds outside poll(): a spin, or a driver call that never
  returns. Waiting for the X server does not count (the watchdog reads the main
  thread's system call from `/proc`), since a stalled or grabbed server freezes
  the desktop whatever rxcom does, and rxcom carries on once it answers; the
  watchdog only warns then. When rxcom's connection closes, the X server undoes
  the redirection, so the desktop is usable meanwhile.
- **Restarts.** Before aborting, the watchdog starts a new rxcom with the same
  options, which backs off (1 s, then 2, 4, 8 and 16) and takes over once the
  stuck one has let go of the screen; after five restarts it gives up. A turn
  stuck while GL or Vulkan was being set up restarts rxcom with the X
  presenter. The watchdog watches start-up too, since a driver can hang there.
- **Fallbacks.** Where GL or Vulkan cannot start, the X server presents, and
  GPU errors rebuild the presenter (6.5).
- **Errors.** The X errors a compositor expects while windows come and go
  (BadWindow, BadDrawable, BadPixmap, BadMatch, BadDamage, BadValue,
  BadPicture) are logged only at trace level.

## 13. Costs

Measured at 3840x2160 and 240 Hz on an RTX 4070 SUPER with NVIDIA's X driver,
unless stated otherwise.

- **The X server:** the composition requests take about 0.25 ms of CPU per
  frame for four windows redrawn in full (on glamor), less when only part of
  the screen changes, and nothing when idle. There are no readbacks and no GLX
  binds.
- **rxcom:** about 0.1 ms of CPU per frame, measured over 670 zoomed 4K frames
  on a nested Xwayland with a release build, fence queries included.
- **The GPU:** the X server's composition of the damaged area, plus, with GL
  or Vulkan, one full-screen pass per frame, about 67 MB of memory traffic at
  4K, roughly 0.13 ms.
- **Memory:** with the X presenter, rxcom holds 4 to 5 MB. With Vulkan, the
  scene (33 MB) and four swapchain images (133 MB) live in video memory, and
  95 to 115 MB are resident, nearly all of it NVIDIA's driver.
- **Idle:** the X and GL presenters draw nothing and use no CPU. Vulkan
  presents at every refresh without drawing: on a nested Xwayland at 249
  presents a second, rxcom used 1.5% of a core and Xwayland 0.5%. While zoomed
  and still, rxcom queries the pointer every 100 ms.

The GPU's power, read from nvidia-smi, over a minute at rest and a minute with
one 1280x720 window redrawn 60 times a second:

| Presenter | At rest | One window animating | Xorg's CPU |
| --- | --- | --- | --- |
| X, ForceFullCompositionPipeline on | 37.2 W, P3 (memory at 5 GHz) | 37.4 W | 8% of a core, always |
| X, ForceFullCompositionPipeline off (tears) | 30.1 W, mostly P5 | 25.2 W, P5 (memory at 810 MHz) | 0.3 to 7% |
| Vulkan, presenting at every refresh | 38.6 W, P0 (memory at 10.5 GHz) | 43.4 W | 4 to 18% |

With a program redrawing about 230 times a second, the X presenter with
ForceFullCompositionPipeline drew 72 W at P0, and GL 42 W at P3 (graphics at
1.3 GHz, memory at 5 GHz). rxcom used 2.2 to 2.5% of a core with GL and 0.5%
with the X presenter, and Xorg 0.8% either way.

ForceFullCompositionPipeline makes NVIDIA's driver compose the screen itself
at every refresh, which is what keeps the X presenter from tearing, and it costs
about as much as Vulkan's presenting at every refresh. The X presenter is the
supported one nonetheless: it works on every server and driver, and needs no
fence and no GPU context in rxcom.

## 14. Rejected designs

- **Composing in Vulkan or GL from per-window textures**, whether imported
  through DRI3, copied through MIT-SHM or bound with GLX: the pinned buffers and
  8 ms imports of 4.1, the readbacks of 4.2, and the vblank waits of 4.3.
- **Waiting on X Sync fences inside the X server** (SyncAwaitFence): the lost
  wakeups of 5.2. DRI3 1.4 syncobjs are not in Xorg 21, and NVIDIA attaches no
  implicit fences to dma-bufs.
- **MAILBOX** does not exist on NVIDIA's xcb surface, and **IMMEDIATE** tears.
- **Opting the Vulkan presenter out of variable refresh:** NVIDIA's Vulkan
  swapchains on X ignore every per-program setting (6.4), and turning variable
  refresh off for the whole screen would take it from games too.
- **EGL** for the GL presenter (7.1).
- **present_id, present_wait and VK_EXT_present_timing**, though NVIDIA's 615
  driver offers them: FIFO_LATEST_READY with a zero-timeout acquire paces
  without them, and nothing needs vblank timestamps yet.
- **Effects and a configuration file:** shadows, fading, blur, rounded corners,
  dimming and window rules are what picom is for.

## 15. Testing

Everything that draws runs on nested X servers, never on a desktop in use: a
probe that imports buffers or composites can freeze a live X server, and the
plane-offset variant of 4.1 produced kernel assertions.

- **`tools/nested-test.sh`** starts an Xvfb (`tools/nested.sh`, with
  `-noreset`, since otherwise Xvfb wipes the atoms and root properties whenever
  the last client disconnects), a wallpaper, an xterm and test windows of known
  pixels (`examples/argbwin.rs`), runs rxcom, and compares screenshots with
  independent computations. The composited screen must equal the X server's
  own rendering outside the ARGB windows; ARGB windows must match the
  premultiplied "over" operator within one level; zoom at 2x, 2.5x, 3x, 4x and
  16x (the property asking for 30518x) must match the mapping of 9.2 computed
  in Python (`tools/zoomcheck.py`), exactly with nearest filtering and within
  four levels with linear filtering, and unzooming must restore the exact
  image. Moved, resized and restacked windows, an animating client, an X
  border changing colour as a window manager's focus does, windows mapped
  again or resized while their clients draw late or in slow parts, full-screen
  windows taking the screen and giving it back (with both values of
  `_NET_WM_BYPASS_COMPOSITOR`), the screen saver, a second instance, SIGTERM
  mid-animation, and an X server stopped for 7 s while zoomed are all checked,
  and the log must hold no warnings.
- **`tools/stress-test.sh`** runs openbox with its stock configuration as a
  reparenting window manager, and drives hundreds of random operations: moves,
  resizes, raises, iconifies, withdraws (which hand clients back to the root),
  kills, zoom changes and wallpaper changes. The final composited screen must
  equal the server's own rendering, and the log must stay clean. It reports
  rxcom's resident memory before and after.
- **`tools/xwayland.sh`** provides a nested server with DRI3: a headless mutter
  on a private D-Bus session, with a rootful Xwayland on a private Wayland
  socket. There the Vulkan presenter runs both suites on NVIDIA's driver with
  the validation layers, whose messages count as failures, and the X server
  takes the Vulkan image through PixmapFromBuffers.
- **GL** runs both suites on Xvfb with Mesa's llvmpipe, which rebinds the
  texture every frame, polls the fences and redraws in full; the suite also
  checks that GL started under the screen saver waits for it to end. On the
  headless Xwayland, NVIDIA's own GLX has no texture_from_pixmap, and Mesa's
  zink on NVIDIA's Vulkan binds the pixmap but samples black, so the start
  test refuses it and the X server presents. Mesa has no
  GL_EXT_x11_sync_object, so only NVIDIA exercises GL's fences.
- **Unit tests** check the zoom's crop and origin against picom-zoom's own
  test, bound for bound, the easing, the window stack, the coverage of a
  drawing in parts, and the buffer age's arithmetic.
- **`RXCOM_TEST_PRESENT=gl` or `vulkan`** makes either suite fail if rxcom
  falls back to the X presenter.

A few things about nested servers trip tests up. Without a window manager the
focus follows the pointer, so xterm redraws its cursor whenever the pointer
enters it, and the tests pin the focus to a window that ignores it. hsetroot
exits without keeping its pixmap, and the next client got the same XID for its
window (section 3). The overlay window is not listed by QueryTree.

Nested servers cannot show flips, pacing, variable refresh, VT switches or
games, so those come from running rxcom on NVIDIA's X driver at 3840x2160 and
240 Hz: the Vulkan route through PixmapFromBuffers (6.1), variable refresh
(6.4), GL's fences at VT switches (7.3) and the power figures (13).

## 16. Code layout

| File | Lines | Role |
| --- | ---: | --- |
| `src/main.rs` | 196 | options, signals, start and restart |
| `src/x.rs` | 468 | connection, extensions, selection, overlay, DRI3, fence, screen saver, helpers |
| `src/windows.rs` | 201 | the stacking list, each window's X resources and stand-in |
| `src/scene.rs` | 238 | the XRender composition, its damage, stand-ins, the X presenter, the background |
| `src/compositor.rs` | 1053 | events, damage, waiting for clients, frames and their pacing, unredirection, VTs, recovery, the loop |
| `src/gpu.rs` | 870 | the Vulkan presenter: the shared scene, the pass, the swapchain and what its images hold |
| `src/gl.rs` | 788 | the GL presenter: GLX, the bound scene, the imported X fences, buffer age |
| `src/zoom.rs` | 115 | the zoom's easing and view |
| `src/watchdog.rs`, `vt.rs`, `logger.rs`, `lib.rs` | 207 | restarts, the VT watch, logging |
| `shaders/` | 28 | the Vulkan pass; GL's is in `gl.rs` |

That is about 4,140 lines of Rust without the unit tests (4,320 with them),
against xcompmgr's 2,400 lines of C, which has shadows and fading but no GL,
Vulkan, zoom, unredirection or recovery. The dependencies are x11rb (on
libxcb), ash, glow, x11-dl, libc, log and anyhow; glslc builds the shaders.
`examples/argbwin.rs` is the test client, and `tools/` holds the nested-server
scripts and the pixel checkers.

## 17. Open questions

- **The fence wait blocks the loop.** With Vulkan, and with GL on Mesa, rxcom
  polls the X server's fence before drawing, and the loop waits meanwhile; on
  NVIDIA's X driver a few waits of 25 to 60 ms were seen, each a gap in the
  presents. Sending the trigger early and collecting it just before submitting
  would overlap the wait with the frame's other work.
- **GL at VT switches** on NVIDIA (7.3).
- **Windows covered by their children** get no Expose and wait 50 ms before
  showing their own pixels (section 8).
- **More than one monitor.** The zoom and the unredirection treat the screen
  as one; per-monitor zoom and unredirection would follow RandR's monitors. One
  swapchain on the overlay can only pace to one refresh rate, so mixed refresh
  rates stay a limit, as they are for picom.
- **A magnified cursor.** rxcom could hide the hardware cursor with
  XFixesHideCursor and draw the cursor's image scaled at the hotspot, still
  with no input transform.
