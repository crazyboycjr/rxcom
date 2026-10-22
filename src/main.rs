use anyhow::{Context, Result, bail};
use rxcom::compositor::{Compositor, Options};
use std::os::fd::RawFd;
use std::process::ExitCode;

const USAGE: &str = "\
rxcom: an X11 compositor on Vulkan

usage: rxcom [options]
  -d, --display NAME    X display (default $DISPLAY)
  --gpu NAME            use the Vulkan device whose name contains NAME
  --no-dri3             copy window contents through MIT-SHM even when DRI3 works
  --no-sync-fence       sample DRI3 windows without waiting on an X Sync fence
  --max-fps N           upper bound on the frame rate (default 240)
  --background RRGGBB   colour when no root pixmap is set (default 303030)
  --validation          enable the Vulkan validation layers
  -v, -vv               more logging (or RXCOM_LOG=debug)
  -h, --help

Zoom: set the root property _PICOM_ZOOM (CARDINAL/32, 16.16 fixed point), e.g.
  xprop -root -f _PICOM_ZOOM 32c -set _PICOM_ZOOM 131072   # 2x
";

fn parse_color(s: &str) -> Result<[f32; 4]> {
    let v = u32::from_str_radix(s.trim_start_matches('#'), 16).context("colour must be RRGGBB")?;
    let c = |shift: u32| ((v >> shift) & 0xff) as f32 / 255.0;
    Ok([c(16), c(8), c(0), 1.0])
}

fn parse_args() -> Result<(Options, u8)> {
    let mut o = Options {
        display: None,
        gpu: std::env::var("RXCOM_GPU").ok(),
        dri3: true,
        sync_fence: true,
        validation: false,
        max_fps: 240.0,
        background: parse_color("303030")?,
    };
    let mut verbose = 0;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "-d" | "--display" => o.display = Some(value()?),
            "--gpu" => o.gpu = Some(value()?),
            "--no-dri3" => o.dri3 = false,
            "--no-sync-fence" => o.sync_fence = false,
            "--max-fps" => o.max_fps = value()?.parse().context("--max-fps")?,
            "--background" => o.background = parse_color(&value()?)?,
            "--validation" => o.validation = true,
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
/// Runs before Vulkan starts any driver threads, so they inherit the mask.
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
