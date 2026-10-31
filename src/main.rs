use anyhow::{Context, Result, bail};
use rxcom::compositor::{Compositor, Options, Presenter};
use std::os::fd::RawFd;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const USAGE: &str = "\
rxcom: an X11 compositor with a full-desktop zoom

usage: rxcom [options]
  -d, --display NAME      X display (default $DISPLAY)
  --present P             what puts the desktop on the screen: x, the X
                          server (the default and the supported presenter),
                          or the experimental gl or vulkan
  --x-present             the same as --present x
  --unredirect-delay MS   give the screen to a full-screen opaque window
                          (a game) after MS milliseconds (default 200)
  --no-unredirect         always composite
  --zoom-filter F         nearest (default) or linear
  --max-fps N             frame rate limit (default: the refresh rate of the
                          fastest lit monitor)
  --present-mode M        with Vulkan: latest (FIFO_LATEST_READY, the default
                          where offered), fifo, relaxed (FIFO_RELAXED) or
                          immediate (tears)
  --no-continuous         with Vulkan, present only when something changed
  --background RRGGBB     colour where no wallpaper is set (default 303030)
  -v, -vv                 more logging (or RXCOM_LOG=debug)
  -h, --help

Zoom: the window manager sets the root property _SCREEN_ZOOM (CARDINAL/32,
16.16 fixed point), as for xcompmgr-zoom; for example 2x, then off:
  xprop -root -f _SCREEN_ZOOM 32c -set _SCREEN_ZOOM 131072
  xprop -root -remove _SCREEN_ZOOM

The X presenter copies each frame onto the screen, which may tear unless the
driver keeps the screen tear-free itself (NVIDIA's
ForceFullCompositionPipeline). gl and vulkan are experimental: they flip at
vblank, so they never tear, but on NVIDIA they have known problems.
  gl      It can stall the X server's switch to another VT; with
          systemd-logind 258 or newer and a rootless X (startx), the
          keyboard and mouse may not come back. GL presents only what
          changed, and sets __GL_VRR_ALLOWED=0 for itself unless that is
          set already.
  vulkan  Its flips engage variable refresh (G-SYNC, FreeSync), which no
          setting turns off for one program, so it presents at every
          refresh, even when nothing changed, and keeps the GPU at its top
          clocks.
";

/// Stops rxcom if one turn of its loop is stuck for longer than this.
const WATCHDOG: Duration = Duration::from_secs(5);

fn parse_args() -> Result<(Options, u8)> {
    let mut o = Options {
        display: None,
        presenter: Presenter::X,
        unredirect_delay: Some(Duration::from_millis(200)),
        linear: false,
        max_fps: None,
        present_mode: None,
        continuous: true,
        background: 0x303030,
    };
    let mut verbose = 0;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "-d" | "--display" => o.display = Some(value()?),
            "--present" => {
                o.presenter = match value()?.as_str() {
                    "x" => Presenter::X,
                    "gl" => Presenter::Gl,
                    "vulkan" => Presenter::Vulkan,
                    p => bail!("--present: {p} is not x, gl or vulkan"),
                }
            }
            "--x-present" => o.presenter = Presenter::X,
            "--unredirect-delay" => {
                o.unredirect_delay = Some(Duration::from_millis(value()?.parse().context("--unredirect-delay")?))
            }
            "--no-unredirect" => o.unredirect_delay = None,
            "--zoom-filter" => {
                o.linear = match value()?.as_str() {
                    "nearest" => false,
                    "linear" => true,
                    f => bail!("--zoom-filter: {f} is neither nearest nor linear"),
                }
            }
            "--max-fps" => o.max_fps = Some(value()?.parse().context("--max-fps")?),
            "--present-mode" => {
                let m = value()?;
                let mode = rxcom::gpu::present_mode(&m);
                o.present_mode =
                    Some(mode.with_context(|| format!("--present-mode {m}: not latest, fifo, relaxed or immediate"))?);
            }
            "--no-continuous" => o.continuous = false,
            "--background" => {
                let v = value()?;
                o.background = u32::from_str_radix(v.trim_start_matches('#'), 16).context("--background RRGGBB")?;
            }
            "-v" => verbose += 1,
            "-vv" => verbose += 2,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            _ => bail!("unknown option {a}\n\n{USAGE}"),
        }
    }
    Ok((o, verbose))
}

/// Blocks SIGINT and SIGTERM and returns a signalfd that reports them. SIGHUP
/// is ignored, so rxcom outlives the terminal that started it. Runs before any
/// other thread starts, so they all inherit the mask.
fn signal_fd() -> Result<RawFd> {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            bail!("pthread_sigmask failed");
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
        if fd < 0 {
            bail!("signalfd: {}", std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

/// Starts compositing. After a restart by the watchdog, backs off first and
/// waits for the stuck instance to let go of the screen. The watchdog already
/// watches each attempt, since a driver can hang while setting up.
fn start(opts: &Options) -> Result<Compositor> {
    let attempt = || {
        rxcom::watchdog::busy();
        let c = Compositor::new(opts);
        rxcom::watchdog::idle();
        c
    };
    let restarts = rxcom::watchdog::restarts();
    if restarts == 0 {
        return attempt();
    }
    std::thread::sleep(Duration::from_secs(1 << (restarts - 1).min(5)));
    let since = Instant::now();
    loop {
        match attempt() {
            Err(e) if e.to_string().contains("another compositor") && since.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(500))
            }
            r => return r,
        }
    }
}

fn main() -> ExitCode {
    let (opts, verbose) = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rxcom: {e:#}");
            return ExitCode::from(2);
        }
    };
    // NVIDIA's OpenGL honours this, as its profiles set it for the compositors
    // they know; libGL is not loaded yet, and no other thread runs.
    if opts.presenter == Presenter::Gl && std::env::var_os("__GL_VRR_ALLOWED").is_none() {
        // SAFETY: the process has no other thread yet.
        unsafe { std::env::set_var("__GL_VRR_ALLOWED", "0") };
    }
    rxcom::logger::init(verbose);
    let experimental = match opts.presenter {
        Presenter::X => None,
        Presenter::Gl => Some("GL"),
        Presenter::Vulkan => Some("Vulkan"),
    };
    if let Some(name) = experimental {
        log::info!("the {name} presenter is experimental; --present x is the supported one");
    }
    let result = signal_fd().and_then(|sigfd| {
        rxcom::watchdog::start(WATCHDOG);
        let mut c = start(&opts)?;
        c.run(sigfd)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
