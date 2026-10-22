//! Desktop zoom: the target comes from the root property _PICOM_ZOOM (16.16 fixed
//! point), the current value eases toward it, and the viewport follows the pointer
//! so the desktop point under the hardware cursor never moves.

use std::time::Instant;

pub const MAX_ZOOM: f64 = 16.0;
/// Time constant of the exponential easing, in seconds.
pub const TAU: f64 = 0.050;
/// Snap to the target once within this distance.
pub const SNAP: f64 = 0.001;

#[derive(Debug)]
pub struct Zoom {
    target: f64,
    current: f64,
    last: Option<Instant>,
}

impl Default for Zoom {
    fn default() -> Self {
        Self::new()
    }
}

impl Zoom {
    pub fn new() -> Self {
        Self { target: 1.0, current: 1.0, last: None }
    }

    /// Decodes the property value: missing or at most 1.0 means no zoom.
    pub fn from_property(value: Option<u32>) -> f64 {
        match value {
            Some(v) if v > 65536 => (v as f64 / 65536.0).min(MAX_ZOOM),
            _ => 1.0,
        }
    }

    pub fn set_target(&mut self, z: f64, now: Instant) {
        if !self.animating() {
            // Start the easing clock now, not at the end of the last animation.
            self.last = Some(now);
        }
        self.target = z.clamp(1.0, MAX_ZOOM);
    }

    pub fn target(&self) -> f64 {
        self.target
    }

    pub fn current(&self) -> f64 {
        self.current
    }

    pub fn animating(&self) -> bool {
        self.current != self.target
    }

    /// True while the zoom pass is needed: zoomed in, or easing in or out.
    pub fn active(&self) -> bool {
        self.current > 1.0 || self.target > 1.0
    }

    pub fn advance(&mut self, now: Instant) {
        if self.animating() {
            let dt = self.last.map_or(0.0, |l| now.saturating_duration_since(l).as_secs_f64());
            self.current = self.target + (self.current - self.target) * (-dt / TAU).exp();
            if (self.current - self.target).abs() < SNAP {
                self.current = self.target;
            }
        }
        self.last = Some(now);
    }

    /// The viewport origin o = p * (1 - 1/z) for pointer position p; a desktop
    /// point q is drawn at (q - o) * z.
    pub fn origin(&self, pointer: (f64, f64)) -> (f64, f64) {
        let k = 1.0 - 1.0 / self.current;
        (pointer.0 * k, pointer.1 * k)
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
    fn eases_and_snaps() {
        let t0 = Instant::now();
        let mut z = Zoom::new();
        z.set_target(2.0, t0);
        z.advance(t0 + Duration::from_millis(50));
        // One time constant covers 1 - 1/e of the distance.
        assert!((z.current() - (2.0 - (-1.0f64).exp())).abs() < 1e-9);
        let mut t = t0 + Duration::from_millis(50);
        for _ in 0..100 {
            t += Duration::from_millis(16);
            z.advance(t);
        }
        assert_eq!(z.current(), 2.0);
        assert!(!z.animating());
    }

    #[test]
    fn idle_time_does_not_jump() {
        let t0 = Instant::now();
        let mut z = Zoom::new();
        z.advance(t0);
        let later = t0 + Duration::from_secs(10);
        z.set_target(4.0, later);
        z.advance(later + Duration::from_millis(1));
        assert!(z.current() < 1.1);
    }

    #[test]
    fn pointer_point_is_fixed() {
        let mut z = Zoom::new();
        let t0 = Instant::now();
        z.set_target(3.0, t0);
        z.advance(t0 + Duration::from_secs(5));
        for p in [(0.0, 0.0), (1919.0, 1079.0), (640.0, 333.0)] {
            let o = z.origin(p);
            let s = ((p.0 - o.0) * z.current(), (p.1 - o.1) * z.current());
            assert!((s.0 - p.0).abs() < 1e-9 && (s.1 - p.1).abs() < 1e-9);
            // The viewport stays inside the desktop.
            assert!(o.0 >= 0.0 && o.0 + 1920.0 / z.current() <= 1920.0 + 1e-9);
        }
    }
}
