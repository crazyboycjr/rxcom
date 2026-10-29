//! Desktop zoom, following picom-zoom exactly so both compositors agree: the
//! target comes from the root property _SCREEN_ZOOM (16.16 fixed point), the
//! level eases toward it in log space, and the view keeps the centre of the
//! pixel under the pointer in place, so input needs no transform.

use std::time::Instant;

pub const MAX_ZOOM: f64 = 16.0;
/// Time constant of the easing toward the target, in seconds.
const TAU: f64 = 0.040;
/// Snap to the target once this close, relatively.
const SNAP: f64 = 0.001;

pub struct Zoom {
    target: f64,
    current: f64,
    last: Instant,
}

/// How a zoomed frame samples the scene: screen pixel s shows the scene at
/// `crop + (s + 0.5 - origin) / scale`, in pixel-centre coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub scale: f64,
    pub crop: [i32; 2],
    pub origin: [i32; 2],
}

impl View {
    pub const IDENTITY: View = View { scale: 1.0, crop: [0, 0], origin: [0, 0] };

    /// The scene point that screen point `s` (pixel-centre coordinates) shows.
    pub fn source(&self, s: [f64; 2]) -> [f64; 2] {
        [0, 1].map(|i| self.crop[i] as f64 + (s[i] - self.origin[i] as f64) / self.scale)
    }
}

/// picom-zoom's renderer_zoom_axis: the part of one axis visible when zoomed,
/// `[crop_start, crop_end)` with a pixel of margin, and where that crop lands on
/// the screen. The origin is rounded to whole pixels, then clamped so every
/// screen pixel samples at least half a pixel inside the crop.
pub fn axis(size: i32, scale: f64, center: f64) -> (i32, i32, i32) {
    let center = center.clamp(0.5, size as f64 - 0.5);
    let view_start = center * (1.0 - 1.0 / scale);
    let crop_start = 0.max(view_start.floor() as i32 - 1);
    let crop_end = size.min((view_start + size as f64 / scale).ceil() as i32 + 1);
    let origin = (crop_start as f64 * scale - center * (scale - 1.0)).round();
    let lowest = (size as f64 - 0.5 - (crop_end - crop_start) as f64 * scale + 0.5 * scale).ceil();
    let highest = (-0.5 * (scale - 1.0)).floor();
    // When no integer origin keeps both edges half a pixel in, which happens when
    // scale is barely above 1, lowest is 0 and comes closest.
    (crop_start, crop_end, origin.min(highest).max(lowest) as i32)
}

impl Zoom {
    /// Starts at the property's level without easing, like picom-zoom after a reset.
    pub fn new(value: Option<u32>, now: Instant) -> Self {
        let z = Self::from_property(value);
        Zoom { target: z, current: z, last: now }
    }

    /// Decodes the property value: missing or below 1.0 means no zoom.
    pub fn from_property(value: Option<u32>) -> f64 {
        value.map_or(1.0, |v| (v as f64 / 65536.0).clamp(1.0, MAX_ZOOM))
    }

    pub fn set_target(&mut self, z: f64, now: Instant) {
        if !self.animating() {
            // Start the easing clock now, not at the end of the last animation.
            self.last = now;
        }
        self.target = z;
    }

    pub fn animating(&self) -> bool {
        self.current != self.target
    }

    /// Zoomed in, or easing in or out: the pointer matters and the view may change.
    pub fn active(&self) -> bool {
        self.current > 1.0 || self.target > 1.0
    }

    pub fn wants_zoom(&self) -> bool {
        self.target > 1.0
    }

    pub fn advance(&mut self, now: Instant) {
        if self.animating() {
            let dt = now.saturating_duration_since(self.last).as_secs_f64();
            let (lt, lc) = (self.target.ln(), self.current.ln());
            self.current = (lc + (lt - lc) * (1.0 - (-dt / TAU).exp())).exp();
            if (self.current / self.target - 1.0).abs() < SNAP {
                self.current = self.target;
            }
        }
        self.last = now;
    }

    /// The view for a screen of `size` with the pointer at `pointer`, or None
    /// when not zoomed.
    pub fn view(&self, size: [u16; 2], pointer: [i16; 2]) -> Option<View> {
        if self.current <= 1.0 {
            return None;
        }
        let mut v = View { scale: self.current, crop: [0; 2], origin: [0; 2] };
        for i in 0..2 {
            let (start, _, origin) = axis(size[i] as i32, self.current, pointer[i] as f64 + 0.5);
            v.crop[i] = start;
            v.origin[i] = origin;
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn property_decoding() {
        assert_eq!(Zoom::from_property(None), 1.0);
        assert_eq!(Zoom::from_property(Some(0)), 1.0);
        assert_eq!(Zoom::from_property(Some(65536)), 1.0);
        assert_eq!(Zoom::from_property(Some(131072)), 2.0);
        assert_eq!(Zoom::from_property(Some(65536 * 40)), MAX_ZOOM);
    }

    #[test]
    fn eases_in_log_space_and_snaps() {
        let t0 = Instant::now();
        let mut z = Zoom::new(None, t0);
        z.set_target(4.0, t0);
        z.advance(t0 + Duration::from_millis(40));
        // One time constant covers 1 - 1/e of the distance in log space.
        let want = (4f64.ln() * (1.0 - (-1f64).exp())).exp();
        assert!((z.current - want).abs() < 1e-9);
        let mut t = t0 + Duration::from_millis(40);
        for _ in 0..100 {
            t += Duration::from_millis(4);
            z.advance(t);
        }
        assert_eq!(z.current, 4.0);
        assert!(!z.animating());
    }

    #[test]
    fn idle_time_does_not_jump() {
        let t0 = Instant::now();
        let mut z = Zoom::new(None, t0);
        z.advance(t0);
        let later = t0 + Duration::from_secs(10);
        z.set_target(4.0, later);
        z.advance(later + Duration::from_millis(1));
        assert!(z.current < 1.1);
    }

    /// picom-zoom's TEST_CASE(renderer_zoom_axis), bound for bound.
    #[test]
    fn axis_bounds() {
        for size in [2, 7, 640, 1080, 1920, 2160, 3840] {
            for scale in [1.0001, 1.001, 1.01, 1.5, 2.0, 2.9, 4.0, 7.3, 16.0] {
                let slack = if (size as f64 - 0.5) * (scale - 1.0) < 1.0 { 0.5 * (scale - 1.0) } else { 1e-9 };
                for p in 0..size {
                    let center = p as f64 + 0.5;
                    let (start, end, origin) = axis(size, scale, center);
                    assert!(0 <= start && start < end && end <= size);
                    let o = origin as f64;
                    assert!((0.5 - o) / scale >= 0.5 - slack);
                    assert!((size as f64 - 0.5 - o) / scale <= (end - start) as f64 - 0.5 + slack);
                    let drift = (o + (center - start as f64) * scale - center).abs();
                    assert!(drift <= 1.0 + 1e-9);
                    if start > 0 && end < size {
                        assert!(drift <= 0.5 + 1e-9);
                    }
                    assert!(origin.abs() as f64 <= 3.0 * scale);
                    assert!((end - start) as f64 * scale <= size as f64 + 4.0 * scale);
                }
            }
        }
    }

    #[test]
    fn pointer_pixel_stays_put() {
        let t0 = Instant::now();
        let z = Zoom::new(Some(3 * 65536), t0);
        for p in [[0i16, 0i16], [1919, 1079], [640, 333]] {
            let v = z.view([1920, 1080], p).unwrap();
            let q = v.source([p[0] as f64 + 0.5, p[1] as f64 + 0.5]);
            // Within a screen pixel at the edges, half of one elsewhere.
            for i in 0..2 {
                assert!((q[i] - (p[i] as f64 + 0.5)).abs() <= 1.0 / 3.0 + 1e-9, "{p:?} -> {q:?}");
            }
        }
    }
}
