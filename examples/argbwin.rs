//! A test client: a top-level window of depth 32 (ARGB) or 24 filled with known
//! pixels, so compositor output can be checked exactly.
//!
//!   argbwin [--geometry WxH+X+Y] [--depth 32|24] [--color AARRGGBB]
//!           [--pattern solid|gradient|quadrants] [--animate] [--fps N] [--name NAME]
//!           [--override-redirect] [--border N] [--blink]
//!           [--no-background] [--delay MS] [--band-delay MS]
//!   argbwin --overlay [--color AARRGGBB]
//!
//! Colours are premultiplied ARGB. `gradient` ramps alpha from 0 at the left to
//! the colour's alpha at the right; `quadrants` paints four opaque colours;
//! `--animate` redraws the window with an opaque bar moving across it, `--fps`
//! times a second (default 20); `--blink` switches the colour of the X border
//! (`--border`, green at first) between green and magenta as often, the way a
//! window manager marks focus. A solid window is drawn with fills, which cost
//! the X server little, so an animated one stands for a GPU-drawn client.
//! `--no-background` leaves the window's background None, so the X server
//! paints nothing when it is mapped or resized, and `--delay` waits that long
//! after each exposure before drawing: a slow client such as a software-rendered
//! Electron app. The window is drawn in bands of rows, one request each, and
//! `--band-delay` pauses between them.
//!
//! `--overlay` instead fills the composite overlay window with the colour and
//! exits. That changes the X server's screen pixmap only, as a presenter that
//! flips leaves it out of date, and the X server copies from it into the pixmap
//! of every window it maps or resizes.

use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeGCAux, ChangeWindowAttributesAux, ColormapAlloc, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, ImageFormat, PropMode, Rectangle, VisualClass, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

struct Args {
    w: u16,
    h: u16,
    x: i16,
    y: i16,
    depth: u8,
    color: u32,
    pattern: String,
    animate: bool,
    fps: f64,
    name: String,
    override_redirect: bool,
    border: u16,
    blink: bool,
    background: bool,
    delay: Duration,
    band_delay: Duration,
    overlay: bool,
}

fn parse() -> Result<Args> {
    let mut a = Args {
        w: 400,
        h: 300,
        x: 100,
        y: 100,
        depth: 32,
        color: 0x80800000,
        pattern: "solid".into(),
        animate: false,
        fps: 20.0,
        name: "argbwin".into(),
        override_redirect: false,
        border: 0,
        blink: false,
        background: true,
        delay: Duration::ZERO,
        band_delay: Duration::ZERO,
        overlay: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().with_context(|| format!("{k} needs a value"));
        match k.as_str() {
            "--geometry" => {
                let g = v()?;
                let (size, pos) = g.split_once('+').context("WxH+X+Y")?;
                let (w, h) = size.split_once('x').context("WxH+X+Y")?;
                let (x, y) = pos.split_once('+').context("WxH+X+Y")?;
                (a.w, a.h, a.x, a.y) = (w.parse()?, h.parse()?, x.parse()?, y.parse()?);
            }
            "--depth" => a.depth = v()?.parse()?,
            "--color" => a.color = u32::from_str_radix(&v()?, 16)?,
            "--pattern" => a.pattern = v()?,
            "--animate" => a.animate = true,
            "--fps" => a.fps = v()?.parse::<f64>()?.max(1.0),
            "--name" => a.name = v()?,
            "--override-redirect" => a.override_redirect = true,
            "--border" => a.border = v()?.parse()?,
            "--blink" => a.blink = true,
            "--no-background" => a.background = false,
            "--delay" => a.delay = Duration::from_millis(v()?.parse()?),
            "--band-delay" => a.band_delay = Duration::from_millis(v()?.parse()?),
            "--overlay" => a.overlay = true,
            _ => bail!("unknown option {k}"),
        }
    }
    Ok(a)
}

/// The X border's two colours, opaque green and magenta.
const BORDER: [u32; 2] = [0xff00c000, 0xffc000c0];

fn pixel(a: &Args, (w, h): (u16, u16), x: u32, y: u32) -> u32 {
    match a.pattern.as_str() {
        "gradient" => {
            let t = x as f32 / (w.max(2) - 1) as f32;
            let scale = |c: u32| ((c & 0xff) as f32 * t).round() as u32;
            (scale(a.color >> 24) << 24) | (scale(a.color >> 16) << 16) | (scale(a.color >> 8) << 8) | scale(a.color)
        }
        "quadrants" => {
            let right = x >= w as u32 / 2;
            let bottom = y >= h as u32 / 2;
            match (right, bottom) {
                (false, false) => 0xffff0000,
                (true, false) => 0xff00ff00,
                (false, true) => 0xff0000ff,
                (true, true) => 0xffffffff,
            }
        }
        _ => a.color,
    }
}

fn main() -> Result<()> {
    let a = parse()?;
    let (conn, screen) = x11rb::connect(None)?;
    let s = &conn.setup().roots[screen];
    if a.overlay {
        let overlay = conn.composite_get_overlay_window(s.root)?.reply()?.overlay_win;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, overlay, &CreateGCAux::new().foreground(a.color & 0xffffff))?;
        let all = Rectangle { x: 0, y: 0, width: s.width_in_pixels, height: s.height_in_pixels };
        conn.poly_fill_rectangle(overlay, gc, &[all])?;
        conn.composite_release_overlay_window(s.root)?;
        conn.get_input_focus()?.reply()?;
        return Ok(());
    }
    let visual = s
        .allowed_depths
        .iter()
        .filter(|d| d.depth == a.depth)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .with_context(|| format!("no TrueColor visual of depth {}", a.depth))?
        .visual_id;
    let cmap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, cmap, s.root, visual)?;
    let win = conn.generate_id()?;
    let mut aux = CreateWindowAux::new()
        .border_pixel(BORDER[0])
        .colormap(cmap)
        .override_redirect(a.override_redirect as u32)
        .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY);
    if a.background {
        aux = aux.background_pixel(0);
    }
    conn.create_window(a.depth, win, s.root, a.x, a.y, a.w, a.h, a.border, WindowClass::INPUT_OUTPUT, visual, &aux)?;
    conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, a.name.as_bytes())?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new())?;
    conn.map_window(win)?;
    conn.flush()?;

    const BAR: u32 = 0xffffff00;
    let mut size = (a.w, a.h);
    let draw = |size: (u16, u16), bar: Option<u16>| -> Result<()> {
        let (w, h) = size;
        if a.pattern == "solid" && a.band_delay.is_zero() {
            let fill = |color: u32, x: u16, width: u16| -> Result<()> {
                conn.change_gc(gc, &ChangeGCAux::new().foreground(color))?;
                conn.poly_fill_rectangle(win, gc, &[Rectangle { x: x as i16, y: 0, width, height: h }])?;
                Ok(())
            };
            fill(a.color, 0, w)?;
            if let Some(b) = bar {
                fill(BAR, b, 20)?;
            }
            conn.flush()?;
            return Ok(());
        }
        let rows_per_req = (60000 / (w as usize * 4)).max(1) as u16;
        let mut y0 = 0u16;
        while y0 < h {
            let rows = rows_per_req.min(h - y0);
            let mut data = Vec::with_capacity(w as usize * rows as usize * 4);
            for y in y0..y0 + rows {
                for x in 0..w {
                    let mut p = pixel(&a, size, x as u32, y as u32);
                    if let Some(b) = bar
                        && x >= b
                        && x < b + 20
                    {
                        p = BAR;
                    }
                    data.extend_from_slice(&p.to_le_bytes());
                }
            }
            conn.put_image(ImageFormat::Z_PIXMAP, win, gc, w, rows, 0, y0 as i16, 0, a.depth, &data)?;
            y0 += rows;
            if !a.band_delay.is_zero() && y0 < h {
                conn.flush()?;
                std::thread::sleep(a.band_delay);
            }
        }
        conn.flush()?;
        Ok(())
    };

    let start = Instant::now();
    let mut next = start;
    let mut exposed: Option<Instant> = None;
    loop {
        while let Some(ev) = conn.poll_for_event()? {
            match ev {
                Event::Expose(e) if e.count == 0 => {
                    exposed.get_or_insert(Instant::now());
                }
                Event::ConfigureNotify(e) => size = (e.width, e.height),
                _ => {}
            }
        }
        if exposed.is_some_and(|t| t.elapsed() >= a.delay) {
            exposed = None;
            draw(size, None)?;
        }
        if (a.animate || a.blink) && Instant::now() >= next {
            let ms = start.elapsed().as_millis() as u32;
            if a.animate {
                // 160 pixels a second, whatever the frame rate.
                let span = size.0.saturating_sub(20).max(1) as u32;
                draw(size, Some((ms * 8 / 50 % span) as u16))?;
            }
            if a.blink {
                let aux = ChangeWindowAttributesAux::new().border_pixel(BORDER[(ms / 50) as usize % 2]);
                conn.change_window_attributes(win, &aux)?;
                conn.flush()?;
            }
            next += Duration::from_secs_f64(1.0 / a.fps);
        }
        let due = if a.animate || a.blink { next.saturating_duration_since(Instant::now()) } else { Duration::MAX };
        std::thread::sleep(due.min(Duration::from_millis(5)));
    }
}
