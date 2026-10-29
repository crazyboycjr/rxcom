use anyhow::{Context, Result, bail};
use rxcom::compositor::{Compositor, Options};
use std::os::fd::RawFd;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const USAGE: &str = "\
rxcom: an X11 compositor with a desktop zoom

usage: rxcom [options]
  -d, --display NAME      X display (default $DISPLAY)
  --x-present             let the X server present (may tear) instead of Vulkan
  --unredirect-delay MS   give the screen to a full-screen opaque window
                          (a game) after MS milliseconds (default 200)
  --no-unredirect         always composite
  --zoom-filter F         nearest (default) or linear
  --max-fps N             frame rate limit (default: the refresh rate of the
                          fastest lit monitor)
  --present-mode M        latest (FIFO_LATEST_READY, the default where offered),
                          fifo, relaxed (FIFO_RELAXED) or immediate (tears)
  --no-continuous         with Vulkan, present only when something changed
  --background RRGGBB     colour where no wallpaper is set (default 303030)
  -v, -vv                 more logging (or RXCOM_LOG=debug)
  -h, --help

Zoom: the window manager sets the root property _SCREEN_ZOOM (CARDINAL/32,
16.16 fixed point), as for xcompmgr-zoom; for example 2x, then off:
  xprop -root -f _SCREEN_ZOOM 32c -set _SCREEN_ZOOM 131072
  xprop -root -remove _SCREEN_ZOOM

The Vulkan presenter presents at every refresh, even when nothing changed, so
that a variable-refresh (G-SYNC, FreeSync) monitor stays at its top rate:
NVIDIA gives Vulkan applications on X no way to opt out of variable refresh.
Presenting an unchanged frame copies nothing.
";

/// Stops rxcom if one turn of its loop is stuck for longer than this.
const WATCHDOG: Duration = Duration::from_secs(5);

fn parse_args() -> Result<(Options, u8)> {
    let mut o = Options {
        display: None,
        vulkan: true,
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
            "--x-present" => o.vulkan = false,
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
/// waits for the stuck instance to let go of the screen.
fn start(opts: &Options) -> Result<Compositor> {
    let restarts = rxcom::watchdog::restarts();
    if restarts == 0 {
        return Compositor::new(opts);
    }
    std::thread::sleep(Duration::from_secs(1 << (restarts - 1).min(5)));
    let since = Instant::now();
    loop {
        match Compositor::new(opts) {
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
    rxcom::logger::init(verbose);
    let result = signal_fd().and_then(|sigfd| {
        let mut c = start(&opts)?;
        rxcom::watchdog::start(WATCHDOG);
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
