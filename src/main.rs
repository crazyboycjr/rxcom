use anyhow::{Context, Result, bail};
use rxcom::compositor::{Compositor, Options};
use std::os::fd::RawFd;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
rxcom: an X11 compositor with a desktop zoom

usage: rxcom [options]
  -d, --display NAME      X display (default $DISPLAY)
  --unredirect-delay MS   give the screen to a full-screen opaque window
                          (a game) after MS milliseconds (default 200)
  --no-unredirect         always composite
  --zoom-filter F         nearest (default) or linear
  --max-fps N             frame rate limit without vblank pacing (default 240)
  --background RRGGBB     colour where no wallpaper is set (default 303030)
  -v, -vv                 more logging (or RXCOM_LOG=debug)
  -h, --help

Zoom: the window manager sets the root property _PICOM_ZOOM (CARDINAL/32,
16.16 fixed point), as for picom-zoom; for example 2x, then off:
  xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 131072
  xprop -root -remove _PICOM_ZOOM
";

/// Stops rxcom if one turn of its loop takes longer than this.
const WATCHDOG: Duration = Duration::from_secs(5);

fn parse_args() -> Result<(Options, u8)> {
    let mut o = Options {
        display: None,
        unredirect_delay: Some(Duration::from_millis(200)),
        zoom_filter: b"nearest",
        max_fps: 240.0,
        background: 0x303030,
    };
    let mut verbose = 0;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "-d" | "--display" => o.display = Some(value()?),
            "--unredirect-delay" => {
                o.unredirect_delay = Some(Duration::from_millis(value()?.parse().context("--unredirect-delay")?))
            }
            "--no-unredirect" => o.unredirect_delay = None,
            "--zoom-filter" => {
                o.zoom_filter = match value()?.as_str() {
                    "nearest" => b"nearest",
                    "linear" | "bilinear" => b"bilinear",
                    f => bail!("--zoom-filter: {f} is neither nearest nor linear"),
                }
            }
            "--max-fps" => o.max_fps = value()?.parse().context("--max-fps")?,
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

/// Blocks SIGINT, SIGTERM and SIGHUP and returns a signalfd that reports them.
/// Runs before any other thread starts, so they all inherit the mask.
fn signal_fd() -> Result<RawFd> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
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
        rxcom::watchdog::start(WATCHDOG);
        let mut c = Compositor::new(&opts)?;
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
