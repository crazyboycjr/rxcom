//! The GL presenter: GLX on the overlay window. The X server composes the
//! desktop into the scene pixmap as for the X presenter, and one texture bound
//! to that pixmap (GLX_EXT_texture_from_pixmap) shows it, 1:1 or zoomed; with
//! swap interval 1 the driver flips at vblank. A frame waits for the X server's
//! composition on the GPU, through X fences imported once with
//! GL_EXT_x11_sync_object, and redraws only what changed since its back buffer
//! was last shown (GLX_EXT_buffer_age). A frame starts only once the GPU is
//! done with what it needs. The one wait on the CPU is inside NVIDIA's
//! glWaitSync, which returns only once the X server has triggered the fence, so
//! the trigger always goes first.

use crate::scene::union;
use crate::x::{Marker, X};
use crate::zoom::View;
use anyhow::{Context, Result, anyhow, bail};
use glow::HasContext;
use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_int, c_uint, c_ulong, c_void};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use x11_dl::glx::{self, GLXContext, GLXFBConfig, arb};
use x11_dl::xlib::{self, Display, Xlib};
use x11rb::connection::Connection;
use x11rb::protocol::sync::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, Rectangle};

const GLX_BIND_TO_TEXTURE_RGB_EXT: c_int = 0x20D0;
const GLX_BIND_TO_TEXTURE_TARGETS_EXT: c_int = 0x20D3;
const GLX_Y_INVERTED_EXT: c_int = 0x20D4;
const GLX_TEXTURE_FORMAT_EXT: c_int = 0x20D5;
const GLX_TEXTURE_TARGET_EXT: c_int = 0x20D6;
const GLX_TEXTURE_FORMAT_RGB_EXT: c_int = 0x20D9;
const GLX_TEXTURE_2D_EXT: c_int = 0x20DC;
const GLX_TEXTURE_2D_BIT_EXT: c_int = 0x2;
const GLX_FRONT_LEFT_EXT: c_int = 0x20DE;
const GLX_BACK_BUFFER_AGE_EXT: c_int = 0x20F4;
const GLX_CONTEXT_ROBUST_ACCESS_BIT_ARB: c_int = 0x4;
const GLX_CONTEXT_RESET_NOTIFICATION_STRATEGY_ARB: c_int = 0x8256;
const GLX_LOSE_CONTEXT_ON_RESET_ARB: c_int = 0x8252;
const GL_SYNC_X11_FENCE_EXT: u32 = 0x90E1;

/// Swaps whose GPU work may still be queued. More, and the driver would block
/// in the swap until a vblank.
const MAX_IN_FLIGHT: usize = 2;
/// X fences in turn; each is reused four compositions later.
const FENCES: usize = 4;
/// A composition fence not triggered after this long is triggered again, in
/// case the X server lost the first trigger.
const FENCE_STUCK: Duration = Duration::from_millis(100);
/// Frames whose damage is kept for the buffer age.
const HISTORY: usize = 4;

const VERT: &str = "#version 330 core
void main() {
    vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}";

// shaders/present.frag's mapping, with GL's window y running up, and the
// texture's rows bottom up unless the binding is y-inverted.
const FRAG: &str = "#version 330 core
uniform vec2 crop;
uniform vec2 origin;
uniform float inv_scale;
uniform bool linear;
uniform float height;
uniform bool flip;
uniform sampler2D scene;
out vec4 color;
void main() {
    vec2 q = crop + (vec2(gl_FragCoord.x, height - gl_FragCoord.y) - origin) * inv_scale;
    ivec2 size = textureSize(scene, 0);
    vec3 c;
    if (linear) {
        vec2 t = q / vec2(size);
        c = textureLod(scene, flip ? vec2(t.x, 1.0 - t.y) : t, 0.0).rgb;
    } else {
        ivec2 p = clamp(ivec2(floor(q)), ivec2(0), size - 1);
        c = texelFetch(scene, flip ? ivec2(p.x, size.y - 1 - p.y) : p, 0).rgb;
    }
    color = vec4(c, 1.0);
}";

/// The GL context was lost (a GPU reset): set the presenter up again.
#[derive(Debug)]
pub struct Lost;

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("the GL context was lost")
    }
}

impl std::error::Error for Lost {}

/// The last X error on the GL side's own connection, where Xlib would
/// otherwise exit the process.
static X_ERROR: AtomicU8 = AtomicU8::new(0);

unsafe extern "C" fn on_x_error(_: *mut Display, e: *mut xlib::XErrorEvent) -> c_int {
    X_ERROR.store(unsafe { (*e).error_code }, Ordering::Relaxed);
    0
}

type CreateContext = unsafe extern "C" fn(*mut Display, GLXFBConfig, GLXContext, c_int, *const c_int) -> GLXContext;
type BindTexImage = unsafe extern "C" fn(*mut Display, glx::GLXDrawable, c_int, *const c_int);
type ReleaseTexImage = unsafe extern "C" fn(*mut Display, glx::GLXDrawable, c_int);
type SwapIntervalExt = unsafe extern "C" fn(*mut Display, glx::GLXDrawable, c_int);
type SwapIntervalMesa = unsafe extern "C" fn(c_uint) -> c_int;
type ImportSync = unsafe extern "system" fn(u32, isize, u32) -> *mut c_void;
type ResetStatus = unsafe extern "system" fn() -> u32;

/// libX11 and libGL, loaded at run time so that only this presenter loads
/// them, and an Xlib connection of GL's own.
struct Base {
    xlib: Xlib,
    glx: glx::Glx,
    dpy: *mut Display,
    ctx: GLXContext,
    window: glx::GLXWindow,
}

impl Base {
    fn open(display: Option<&str>) -> Result<Base> {
        let xlib = Xlib::open().context("cannot load libX11")?;
        let glx = glx::Glx::open().context("cannot load libGL")?;
        let name = display.map(CString::new).transpose()?;
        let dpy = unsafe { (xlib.XOpenDisplay)(name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr())) };
        if dpy.is_null() {
            bail!("a second X connection for GL");
        }
        unsafe { (xlib.XSetErrorHandler)(Some(on_x_error)) };
        Ok(Base { xlib, glx, dpy, ctx: std::ptr::null_mut(), window: 0 })
    }

    fn proc(&self, name: &CStr) -> *const c_void {
        unsafe { (self.glx.glXGetProcAddressARB)(name.as_ptr().cast()) }
            .map_or(std::ptr::null(), |f| f as *const c_void)
    }

    /// An extension's function, or an error naming it.
    fn ext<F: Copy>(&self, name: &CStr) -> Result<F> {
        let p = self.proc(name);
        if p.is_null() {
            bail!("no {}", name.to_string_lossy());
        }
        // SAFETY: F is the function pointer type the extension defines.
        Ok(unsafe { std::mem::transmute_copy::<*const c_void, F>(&p) })
    }

    fn attr(&self, config: GLXFBConfig, attribute: c_int) -> c_int {
        let mut v = 0;
        unsafe { (self.glx.glXGetFBConfigAttrib)(self.dpy, config, attribute, &mut v) };
        v
    }

    /// An X error caught on this connection since the last call.
    fn x_errors(&self, what: &str) -> Result<()> {
        unsafe { (self.xlib.XSync)(self.dpy, xlib::False) };
        match X_ERROR.swap(0, Ordering::Relaxed) {
            0 => Ok(()),
            code => bail!("X error {code} {what}"),
        }
    }

    /// A double-buffered config for the overlay window (the root's visual), and
    /// one that binds a pixmap of the root's depth as an RGB texture.
    fn configs(&self, x: &X) -> Result<(GLXFBConfig, GLXFBConfig)> {
        let mut n = 0;
        let list = unsafe { (self.glx.glXGetFBConfigs)(self.dpy, x.screen as c_int, &mut n) };
        if list.is_null() {
            bail!("no GLX configs");
        }
        let configs = unsafe { std::slice::from_raw_parts(list, n as usize) }.to_vec();
        unsafe { (self.xlib.XFree)(list.cast()) };
        let depth = c_int::from(x.conn.setup().roots[x.screen].root_depth);
        let visual_depth = |c| unsafe {
            let vi = (self.glx.glXGetVisualFromFBConfig)(self.dpy, c);
            if vi.is_null() {
                return 0;
            }
            let d = (*vi).depth;
            (self.xlib.XFree)(vi.cast());
            d
        };
        // Fewest unneeded buffers first.
        let extra = |c| self.attr(c, glx::GLX_DEPTH_SIZE) + self.attr(c, glx::GLX_STENCIL_SIZE);
        let window = configs
            .iter()
            .copied()
            .filter(|&c| {
                self.attr(c, glx::GLX_VISUAL_ID) as u32 == x.root_visual
                    && self.attr(c, glx::GLX_DOUBLEBUFFER) != 0
                    && self.attr(c, glx::GLX_DRAWABLE_TYPE) & glx::GLX_WINDOW_BIT != 0
                    && self.attr(c, glx::GLX_RENDER_TYPE) & glx::GLX_RGBA_BIT != 0
            })
            .min_by_key(|&c| extra(c))
            .context("no double-buffered GLX config for the root's visual")?;
        let pixmap = configs
            .iter()
            .copied()
            .filter(|&c| {
                self.attr(c, GLX_BIND_TO_TEXTURE_RGB_EXT) != 0
                    && self.attr(c, GLX_BIND_TO_TEXTURE_TARGETS_EXT) & GLX_TEXTURE_2D_BIT_EXT != 0
                    && self.attr(c, glx::GLX_DRAWABLE_TYPE) & glx::GLX_PIXMAP_BIT != 0
                    && visual_depth(c) == depth
            })
            .min_by_key(|&c| (self.attr(c, glx::GLX_DOUBLEBUFFER), extra(c)))
            .context("no GLX config binds a pixmap of the root's depth to a 2D texture")?;
        Ok((window, pixmap))
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                (self.glx.glXMakeContextCurrent)(self.dpy, 0, 0, std::ptr::null_mut());
                (self.glx.glXDestroyContext)(self.dpy, self.ctx);
            }
            if self.window != 0 {
                (self.glx.glXDestroyWindow)(self.dpy, self.window);
            }
            (self.xlib.XCloseDisplay)(self.dpy);
        }
    }
}

struct Uniforms {
    crop: Option<glow::UniformLocation>,
    origin: Option<glow::UniformLocation>,
    inv_scale: Option<glow::UniformLocation>,
    linear: Option<glow::UniformLocation>,
    height: Option<glow::UniformLocation>,
    flip: Option<glow::UniformLocation>,
}

enum FenceState {
    Ready,
    Triggered(Instant),
    /// Reset, once the X server has processed the reset: a GPU wait issued
    /// before then could still see the last trigger.
    Reset(Marker),
}

struct Fence {
    id: u32,
    sync: glow::Fence,
    state: FenceState,
}

pub struct Gl {
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    texture: glow::Texture,
    uniforms: Uniforms,
    bind_tex: BindTexImage,
    release_tex: ReleaseTexImage,
    reset_status: Option<ResetStatus>,
    /// The scene bound as the texture.
    pixmap: glx::GLXPixmap,
    size: (u16, u16),
    /// The texture's rows run bottom up (the binding is not y-inverted).
    flip: bool,
    /// The driver copies the pixmap when binding it, so every frame rebinds.
    rebind: bool,
    buffer_age: bool,
    fences: Vec<Fence>,
    next_fence: usize,
    reserved: Option<usize>,
    /// Signalled once the GPU has run each swap still counted as in flight,
    /// and once it has finished reading the scene for the last frame.
    in_flight: VecDeque<glow::Fence>,
    reads: Option<glow::Fence>,
    /// The damage of the frames shown last, newest first; None: everything.
    history: VecDeque<Option<Rectangle>>,
    /// Since the last `take_stats`: partial redraws, the slowest swap call and
    /// the slowest wait for an X fence.
    partial: u32,
    slowest_swap: Duration,
    slowest_wait: Duration,
    stuck_logged: bool,
    pub route: String,
    // Dropped last: the context and the connection.
    base: Base,
}

impl Gl {
    /// GLX on the overlay window of `x`'s screen, with `scene` (a pixmap of
    /// the root's depth and the screen's size) bound as the texture it shows.
    pub fn new(x: &X, display: Option<&str>, scene: u32) -> Result<Gl> {
        let mut base = Base::open(display)?;
        let exts = unsafe { (base.glx.glXQueryExtensionsString)(base.dpy, x.screen as c_int) };
        let exts =
            if exts.is_null() { String::new() } else { unsafe { CStr::from_ptr(exts) }.to_string_lossy().into() };
        let has = |e: &str| exts.split_whitespace().any(|x| x == e);
        for e in ["GLX_EXT_texture_from_pixmap", "GLX_ARB_create_context", "GLX_ARB_create_context_profile"] {
            if !has(e) {
                bail!("no {e}");
            }
        }
        let (window_config, pixmap_config) = base.configs(x)?;
        let robust = has("GLX_ARB_create_context_robustness");
        let mut attrs = vec![
            arb::GLX_CONTEXT_MAJOR_VERSION_ARB,
            3,
            arb::GLX_CONTEXT_MINOR_VERSION_ARB,
            3,
            arb::GLX_CONTEXT_PROFILE_MASK_ARB,
            arb::GLX_CONTEXT_CORE_PROFILE_BIT_ARB,
        ];
        // Robust, so that a GPU reset reads as a lost context, but never with
        // GLX_NV_robustness_video_memory_purge: without it the driver keeps
        // our memory across VT switches rather than report a reset.
        if robust {
            attrs.extend([
                arb::GLX_CONTEXT_FLAGS_ARB,
                GLX_CONTEXT_ROBUST_ACCESS_BIT_ARB,
                GLX_CONTEXT_RESET_NOTIFICATION_STRATEGY_ARB,
                GLX_LOSE_CONTEXT_ON_RESET_ARB,
            ]);
        }
        attrs.push(0);
        let create: CreateContext = base.ext(c"glXCreateContextAttribsARB")?;
        base.ctx = unsafe { create(base.dpy, window_config, std::ptr::null_mut(), 1, attrs.as_ptr()) };
        base.x_errors("creating the GL context")?;
        if base.ctx.is_null() {
            bail!("glXCreateContextAttribsARB failed");
        }
        base.window =
            unsafe { (base.glx.glXCreateWindow)(base.dpy, window_config, x.overlay as c_ulong, std::ptr::null()) };
        base.x_errors("creating the GLX window")?;
        if unsafe { (base.glx.glXMakeContextCurrent)(base.dpy, base.window, base.window, base.ctx) } == 0 {
            bail!("glXMakeContextCurrent failed");
        }
        if has("GLX_EXT_swap_control") {
            let set: SwapIntervalExt = base.ext(c"glXSwapIntervalEXT")?;
            unsafe { set(base.dpy, base.window, 1) };
        } else if has("GLX_MESA_swap_control") {
            let set: SwapIntervalMesa = base.ext(c"glXSwapIntervalMESA")?;
            unsafe { set(1) };
        }
        let gl = unsafe { glow::Context::from_loader_function_cstr(|n| base.proc(n)) };
        let (program, vao, texture, uniforms) = unsafe { pass(&gl)? };
        let mut me = Gl {
            bind_tex: base.ext(c"glXBindTexImageEXT")?,
            release_tex: base.ext(c"glXReleaseTexImageEXT")?,
            reset_status: if robust { Some(base.ext(c"glGetGraphicsResetStatusARB")?) } else { None },
            gl,
            program,
            vao,
            texture,
            uniforms,
            pixmap: 0,
            size: (x.width, x.height),
            flip: base.attr(pixmap_config, GLX_Y_INVERTED_EXT) == 0,
            rebind: false,
            buffer_age: has("GLX_EXT_buffer_age"),
            fences: Vec::new(),
            next_fence: 0,
            reserved: None,
            in_flight: VecDeque::new(),
            reads: None,
            history: VecDeque::new(),
            partial: 0,
            slowest_swap: Duration::ZERO,
            slowest_wait: Duration::ZERO,
            stuck_logged: false,
            route: String::new(),
            base,
        };
        // From here on Drop releases what has been made.
        me.rebind = !me.bind_scene(x, pixmap_config, scene)?;
        if me.gl.supported_extensions().contains("GL_EXT_x11_sync_object") {
            me.import_fences(x)?;
            me.check_fences(x)?;
        }
        let sync = me.waits_on_gpu();
        me.base.x_errors("setting GL up")?;
        let renderer = unsafe { me.gl.get_parameter_string(glow::RENDERER) };
        let choose = |on: bool, yes: &'static str, no: &'static str| if on { yes } else { no };
        me.route = format!(
            "GL on {renderer}: the scene's texture {}, {}, {}, {}",
            choose(me.rebind, "rebound every frame", "never rebound"),
            choose(sync, "X fences waited for on the GPU", "X fences polled"),
            choose(me.buffer_age, "buffer age", "full redraws"),
            choose(robust, "robust", "not robust"),
        );
        Ok(me)
    }

    /// Binds the scene as the texture, and finds out whether the texture then
    /// shows the X server's later drawing without a rebind. It does where the
    /// binding shares the pixmap's memory, which is then never rebound, since
    /// NVIDIA's binds can wait for a vblank; it does not where the driver
    /// copies the pixmap when binding it (Mesa's software rasteriser).
    fn bind_scene(&mut self, x: &X, config: GLXFBConfig, scene: u32) -> Result<bool> {
        let fence = x.create_fence()?;
        let size = self.size;
        let fill = |rgb: u32| -> Result<()> {
            let gc = x.conn.generate_id()?;
            x.conn.create_gc(gc, scene, &CreateGCAux::new().foreground(rgb))?;
            let all = Rectangle { x: 0, y: 0, width: size.0, height: size.1 };
            x.conn.poly_fill_rectangle(scene, gc, &[all])?;
            x.conn.free_gc(gc)?;
            if !x.sync_gpu(fence, Duration::from_secs(1))? {
                bail!("the X server's fence did not trigger");
            }
            Ok(())
        };
        let (before, after) = (0x0000ff, 0x00ff00);
        let result = (|| -> Result<bool> {
            fill(before)?;
            // The X server must know the pixmap before this connection names it.
            x.conn.get_input_focus()?.reply()?;
            let attrs =
                [GLX_TEXTURE_TARGET_EXT, GLX_TEXTURE_2D_EXT, GLX_TEXTURE_FORMAT_EXT, GLX_TEXTURE_FORMAT_RGB_EXT, 0];
            let b = &self.base;
            self.pixmap = unsafe { (b.glx.glXCreatePixmap)(b.dpy, config, scene as c_ulong, attrs.as_ptr()) };
            b.x_errors("binding the scene")?;
            unsafe { (self.bind_tex)(b.dpy, self.pixmap, GLX_FRONT_LEFT_EXT, std::ptr::null()) };
            b.x_errors("binding the scene")?;
            fill(after)?;
            match self.sample() {
                s if s == after => Ok(true),
                s if s == before => {
                    self.rebind();
                    match self.sample() {
                        s if s == after => Ok(false),
                        s => bail!(
                            "the scene's texture shows 0x{s:06x} after a rebind where the X server drew 0x{after:06x}"
                        ),
                    }
                }
                s => bail!("the scene's texture shows 0x{s:06x} where the X server drew 0x{after:06x}"),
            }
        })();
        x.conn.sync_destroy_fence(fence)?;
        result
    }

    fn rebind(&self) {
        unsafe {
            (self.release_tex)(self.base.dpy, self.pixmap, GLX_FRONT_LEFT_EXT);
            (self.bind_tex)(self.base.dpy, self.pixmap, GLX_FRONT_LEFT_EXT, std::ptr::null());
        }
    }

    /// The scene's top-left pixel as a frame samples it, drawn into a back
    /// buffer that is never shown.
    fn sample(&self) -> u32 {
        let corner = Rectangle { x: 0, y: 0, width: 1, height: 1 };
        let mut px = [0u8; 4];
        unsafe {
            self.pass(View::IDENTITY, false, Some(corner));
            let data = glow::PixelPackData::Slice(Some(&mut px));
            self.gl.read_pixels(0, i32::from(self.size.1) - 1, 1, 1, glow::BGRA, glow::UNSIGNED_BYTE, data);
        }
        u32::from_le_bytes([px[0], px[1], px[2], 0])
    }

    fn import_fences(&mut self, x: &X) -> Result<()> {
        let import: ImportSync = self.base.ext(c"glImportSyncEXT")?;
        let ids = (0..FENCES).map(|_| x.create_fence()).collect::<Result<Vec<_>>>()?;
        x.conn.get_input_focus()?.reply()?;
        for id in ids {
            let sync = unsafe { import(GL_SYNC_X11_FENCE_EXT, id as isize, 0) };
            if sync.is_null() {
                let _ = x.conn.sync_destroy_fence(id);
                bail!("glImportSyncEXT failed");
            }
            self.fences.push(Fence { id, sync: glow::NativeFence(sync.cast()), state: FenceState::Ready });
        }
        Ok(())
    }

    /// Checks that an imported fence works as frames use it, which the
    /// extension leaves open after a reset: a GPU wait issued after a trigger
    /// ends, and again after a reset. Where it does not, GL is refused.
    fn check_fences(&self, x: &X) -> Result<()> {
        let (gl, f) = (&self.gl, &self.fences[0]);
        for _ in 0..2 {
            x.conn.sync_trigger_fence(f.id)?;
            x.conn.flush()?;
            let released = unsafe {
                gl.wait_sync(f.sync, 0, u64::MAX);
                let done = gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).map_err(|e| anyhow!(e))?;
                let r = gl.client_wait_sync(done, glow::SYNC_FLUSH_COMMANDS_BIT, 1_000_000_000);
                gl.delete_sync(done);
                matches!(r, glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED)
            };
            if !released {
                bail!("a GPU wait for an X fence did not end after its trigger");
            }
            x.conn.sync_reset_fence(f.id)?;
            x.reached(x.marker()?)?;
        }
        Ok(())
    }

    /// Whether the GPU has run a fence.
    fn signalled(&self, f: glow::Fence) -> bool {
        unsafe { self.gl.get_sync_status(f) == glow::SIGNALED }
    }

    /// Whether a frame can start without anything waiting on the CPU: few
    /// enough swaps in flight and, when the X server is about to compose into
    /// the scene, GL done reading it and an X fence free to wait for.
    pub fn ready(&mut self, x: &X, compose: bool) -> Result<bool> {
        while let Some(&f) = self.in_flight.front()
            && self.signalled(f)
        {
            unsafe { self.gl.delete_sync(f) };
            self.in_flight.pop_front();
        }
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            return Ok(false);
        }
        if !compose || self.reserved.is_some() {
            return Ok(true);
        }
        if self.reads.is_some_and(|f| !self.signalled(f)) {
            return Ok(false);
        }
        if self.fences.is_empty() {
            return Ok(true);
        }
        self.recycle_fences(x)?;
        let i = self.next_fence;
        match std::mem::replace(&mut self.fences[i].state, FenceState::Ready) {
            FenceState::Ready => {}
            FenceState::Reset(m) => x.reached(m)?,
            triggered @ FenceState::Triggered(_) => {
                self.fences[i].state = triggered;
                return Ok(false);
            }
        }
        (self.reserved, self.next_fence) = (Some(i), (i + 1) % self.fences.len());
        Ok(true)
    }

    /// Resets the fences the X server has triggered since, for later frames.
    fn recycle_fences(&mut self, x: &X) -> Result<()> {
        let now = Instant::now();
        for i in 0..self.fences.len() {
            let FenceState::Triggered(at) = self.fences[i].state else { continue };
            let (id, sync) = (self.fences[i].id, self.fences[i].sync);
            if self.signalled(sync) {
                x.conn.sync_reset_fence(id)?;
                self.fences[i].state = FenceState::Reset(x.marker()?);
            } else if now - at > FENCE_STUCK {
                if !self.stuck_logged {
                    let ms = FENCE_STUCK.as_millis();
                    log::warn!("the X server's fence did not trigger within {ms} ms; triggering it again");
                    self.stuck_logged = true;
                }
                x.conn.sync_trigger_fence(id)?;
                self.fences[i].state = FenceState::Triggered(now);
            }
        }
        Ok(())
    }

    /// Whether the GPU waits for the X server's composition, so that nothing
    /// needs to poll for it.
    pub fn waits_on_gpu(&self) -> bool {
        !self.fences.is_empty()
    }

    /// After a VT switch the back buffers' contents are unknown.
    pub fn forget_history(&mut self) {
        self.history.clear();
    }

    /// Shows the scene with `view`. `damage` is what the X server just composed
    /// into it, if anything; `full` asks for everything to be redrawn. A frame
    /// redraws only what its back buffer lacks, and one that lacks nothing is
    /// not presented. Returns whether it presented.
    pub fn draw(&mut self, x: &X, view: View, linear: bool, damage: Option<Rectangle>, full: bool) -> Result<bool> {
        if let Some(i) = self.reserved.take() {
            let f = &mut self.fences[i];
            x.conn.sync_trigger_fence(f.id)?;
            x.conn.flush()?;
            let start = Instant::now();
            unsafe { self.gl.wait_sync(f.sync, 0, u64::MAX) };
            self.slowest_wait = self.slowest_wait.max(start.elapsed());
            f.state = FenceState::Triggered(Instant::now());
        }
        if self.rebind && damage.is_some() {
            self.rebind();
        }
        let area = match if full { None } else { self.stale_area(damage) } {
            Some(None) => return Ok(false),
            Some(area) => area,
            None => None,
        };
        self.partial += area.is_some() as u32;
        unsafe { self.pass(view, linear, area) };
        let gl = &self.gl;
        unsafe {
            if let Some(f) = self.reads.take() {
                gl.delete_sync(f);
            }
            self.reads = Some(gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).map_err(|e| anyhow!(e))?);
            let start = Instant::now();
            (self.base.glx.glXSwapBuffers)(self.base.dpy, self.base.window);
            self.slowest_swap = self.slowest_swap.max(start.elapsed());
            self.in_flight.push_back(gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).map_err(|e| anyhow!(e))?);
            gl.flush();
        }
        self.history.push_front(if full { None } else { damage });
        self.history.truncate(HISTORY);
        if self.reset_status.is_some_and(|status| unsafe { status() } != glow::NO_ERROR) {
            return Err(Lost.into());
        }
        Ok(true)
    }

    /// Draws the scene with `view` into the back buffer, only within `area`
    /// (in the screen's coordinates) when given.
    unsafe fn pass(&self, view: View, linear: bool, area: Option<Rectangle>) {
        let (gl, u) = (&self.gl, &self.uniforms);
        let (w, h) = (i32::from(self.size.0), i32::from(self.size.1));
        unsafe {
            gl.viewport(0, 0, w, h);
            match area {
                Some(r) => {
                    gl.enable(glow::SCISSOR_TEST);
                    let (rx, ry, rw, rh) = (i32::from(r.x), i32::from(r.y), i32::from(r.width), i32::from(r.height));
                    gl.scissor(rx, h - ry - rh, rw, rh);
                }
                None => gl.disable(glow::SCISSOR_TEST),
            }
            gl.use_program(Some(self.program));
            gl.uniform_2_f32(u.crop.as_ref(), view.crop[0] as f32, view.crop[1] as f32);
            gl.uniform_2_f32(u.origin.as_ref(), view.origin[0] as f32, view.origin[1] as f32);
            gl.uniform_1_f32(u.inv_scale.as_ref(), (1.0 / view.scale) as f32);
            gl.uniform_1_i32(u.linear.as_ref(), linear as i32);
            gl.uniform_1_f32(u.height.as_ref(), h as f32);
            gl.uniform_1_i32(u.flip.as_ref(), self.flip as i32);
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }

    /// What the back buffer lacks: the new damage and that of the frames shown
    /// since it was. None: everything (no age, an unknown one, or older than
    /// the history); Some(None): nothing.
    fn stale_area(&self, damage: Option<Rectangle>) -> Option<Option<Rectangle>> {
        if !self.buffer_age {
            return None;
        }
        let mut age: c_uint = 0;
        unsafe { (self.base.glx.glXQueryDrawable)(self.base.dpy, self.base.window, GLX_BACK_BUFFER_AGE_EXT, &mut age) };
        lacking(&self.history, age, damage)
    }

    /// Partial redraws, the slowest swap call and the slowest fence wait since
    /// the last call.
    pub fn take_stats(&mut self) -> (u32, Duration, Duration) {
        use std::mem::take;
        (take(&mut self.partial), take(&mut self.slowest_swap), take(&mut self.slowest_wait))
    }

    /// Destroys the X fences, which belong to rxcom's own connection.
    pub fn release(&mut self, x: &X) {
        for f in self.fences.drain(..) {
            unsafe { self.gl.delete_sync(f.sync) };
            if let FenceState::Reset(m) = f.state {
                let _ = x.reached(m);
            }
            let _ = x.conn.sync_destroy_fence(f.id);
        }
    }
}

/// What a back buffer `age` frames old lacks: the new `damage` and that of the
/// frames shown since, which `history` holds newest first. None: everything
/// (age 0 is unknown, or it is older than the history); Some(None): nothing.
fn lacking(history: &VecDeque<Option<Rectangle>>, age: c_uint, damage: Option<Rectangle>) -> Option<Option<Rectangle>> {
    let missed = (age as usize).checked_sub(1)?;
    if missed > history.len() {
        return None;
    }
    let mut area = damage;
    for &d in history.iter().take(missed) {
        let d = d?;
        area = Some(area.map_or(d, |a| union(a, d)));
    }
    Some(area)
}

/// The program that draws the scene, its vertex array (empty: the vertices
/// come from gl_VertexID), the scene's texture and the uniforms.
unsafe fn pass(gl: &glow::Context) -> Result<(glow::Program, glow::VertexArray, glow::Texture, Uniforms)> {
    unsafe {
        let program = gl.create_program().map_err(|e| anyhow!(e))?;
        for (kind, src) in [(glow::VERTEX_SHADER, VERT), (glow::FRAGMENT_SHADER, FRAG)] {
            let s = gl.create_shader(kind).map_err(|e| anyhow!(e))?;
            gl.shader_source(s, src);
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                bail!("GLSL: {}", gl.get_shader_info_log(s));
            }
            gl.attach_shader(program, s);
            gl.delete_shader(s);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            bail!("GLSL: {}", gl.get_program_info_log(program));
        }
        let at = |name| gl.get_uniform_location(program, name);
        let uniforms = Uniforms {
            crop: at("crop"),
            origin: at("origin"),
            inv_scale: at("inv_scale"),
            linear: at("linear"),
            height: at("height"),
            flip: at("flip"),
        };
        let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
        let texture = gl.create_texture().map_err(|e| anyhow!(e))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        for (p, v) in [
            (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
            (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
        }
        Ok((program, vao, texture, uniforms))
    }
}

impl Drop for Gl {
    fn drop(&mut self) {
        let (gl, b) = (&self.gl, &self.base);
        unsafe {
            for f in self.in_flight.drain(..).chain(self.reads.take()) {
                gl.delete_sync(f);
            }
            for f in self.fences.drain(..) {
                gl.delete_sync(f.sync);
            }
            if self.pixmap != 0 {
                (self.release_tex)(b.dpy, self.pixmap, GLX_FRONT_LEFT_EXT);
                (b.glx.glXDestroyPixmap)(b.dpy, self.pixmap);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Area = Option<Option<(i16, i16, u16, u16)>>;

    fn r(x: i16, y: i16, width: u16, height: u16) -> Rectangle {
        Rectangle { x, y, width, height }
    }

    /// `lacking`, with rectangles as tuples, which compare.
    fn lacks(history: &[Option<Rectangle>], age: c_uint, damage: Option<Rectangle>) -> Area {
        let history = history.iter().copied().collect();
        lacking(&history, age, damage).map(|a| a.map(|r| (r.x, r.y, r.width, r.height)))
    }

    #[test]
    fn unknown_age_redraws_everything() {
        assert_eq!(lacks(&[], 0, Some(r(0, 0, 1, 1))), None);
    }

    #[test]
    fn last_frame_needs_only_the_new_damage() {
        let history = [Some(r(50, 50, 10, 10))];
        assert_eq!(lacks(&history, 1, Some(r(0, 0, 4, 4))), Some(Some((0, 0, 4, 4))));
        assert_eq!(lacks(&history, 1, None), Some(None));
    }

    #[test]
    fn older_buffers_add_the_frames_shown_since() {
        let history = [Some(r(10, 10, 5, 5)), Some(r(100, 0, 10, 20))];
        assert_eq!(lacks(&history, 2, Some(r(0, 0, 4, 4))), Some(Some((0, 0, 15, 15))));
        assert_eq!(lacks(&history, 3, None), Some(Some((10, 0, 100, 20))));
    }

    #[test]
    fn a_full_frame_or_too_old_a_buffer_redraws_everything() {
        let history = [Some(r(10, 10, 5, 5)), None];
        assert_eq!(lacks(&history, 2, None), Some(Some((10, 10, 5, 5))));
        assert_eq!(lacks(&history, 3, None), None);
        assert_eq!(lacks(&history, 4, None), None);
    }
}
