//! Dynamic UI DPI scaling authority for `uiserver`.
//!
//! Layout metrics, font rasterization, and Wayland `wl_output` scale all read
//! from this module so changing the display scale dynamically at runtime
//! updates every subsystem coherently without restarting the server.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub(crate) const DEFAULT_SCALE_PERCENT: u32 = 200;
pub(crate) const MIN_SCALE_PERCENT: u32 = 50;
pub(crate) const MAX_SCALE_PERCENT: u32 = 400;

/// Display scale factor expressed as a percentage (100 = 1x, 200 = 2x HiDPI).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UiScale {
    percent: u32,
}

impl UiScale {
    pub(crate) const fn new(percent: u32) -> Option<Self> {
        if percent >= MIN_SCALE_PERCENT && percent <= MAX_SCALE_PERCENT {
            Some(Self { percent })
        } else {
            None
        }
    }

    pub(crate) const fn default_scale() -> Self {
        Self {
            percent: DEFAULT_SCALE_PERCENT,
        }
    }

    pub(crate) const fn percent(self) -> u32 {
        self.percent
    }

    /// Converts a logical pixel count (at 100% DPI) to physical display pixels.
    /// Non-zero logical values always produce at least 1 physical pixel.
    pub(crate) const fn px(self, logical: usize) -> usize {
        if logical == 0 {
            return 0;
        }
        let scaled = ((logical as u64).saturating_mul(self.percent as u64).saturating_add(50)) / 100;
        if scaled == 0 {
            1
        } else {
            scaled as usize
        }
    }

    /// Integer scale advertised via `wl_output.scale` (Wayland core protocol
    /// uses positive integer output scales: 100% -> 1, 200% -> 2, etc.).
    pub(crate) const fn wayland_output_scale(self) -> i32 {
        let rounded = (self.percent + 50) / 100;
        if rounded < 1 {
            1
        } else if rounded > 4 {
            4
        } else {
            rounded as i32
        }
    }
}

impl Default for UiScale {
    fn default() -> Self {
        Self::default_scale()
    }
}

static CURRENT_SCALE_PERCENT: AtomicU32 = AtomicU32::new(DEFAULT_SCALE_PERCENT);
static SCALE_REVISION: AtomicU64 = AtomicU64::new(1);

/// Active UI scale.
pub(crate) fn current() -> UiScale {
    // ORDERING: Relaxed read of the single-word scale percentage; callers that
    // need cross-cache coherence synchronize via `revision()` / UI-thread promotion.
    let percent = CURRENT_SCALE_PERCENT.load(Ordering::Relaxed);
    UiScale::new(percent).unwrap_or_else(UiScale::default_scale)
}

/// Monotonic revision incremented on every dynamic scale change.
pub(crate) fn revision() -> u64 {
    // ORDERING: Acquire pairs with the Release increment in `set`.
    SCALE_REVISION.load(Ordering::Acquire)
}

/// Convenience helper: scale a 100%-logical pixel metric by the active UI scale.
pub(crate) fn px(logical: usize) -> usize {
    current().px(logical)
}

/// Updates the active UI scale. Returns `true` if the scale changed.
pub(crate) fn set(scale: UiScale) -> bool {
    // ORDERING: AcqRel swaps the scale percentage atomically before bumping the
    // revision counter below.
    let previous = CURRENT_SCALE_PERCENT.swap(scale.percent(), Ordering::AcqRel);
    if previous == scale.percent() {
        return false;
    }
    // ORDERING: Release publishes the updated scale to any reader observing
    // the new revision.
    SCALE_REVISION.fetch_add(1, Ordering::Release);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_bounds_and_rounding_are_exact() {
        assert!(UiScale::new(49).is_none());
        assert!(UiScale::new(401).is_none());
        let s100 = UiScale::new(100).unwrap();
        let s150 = UiScale::new(150).unwrap();
        let s200 = UiScale::new(200).unwrap();
        assert_eq!(s100.px(0), 0);
        assert_eq!(s100.px(1), 1);
        assert_eq!(s100.px(36), 36);
        assert_eq!(s150.px(36), 54);
        assert_eq!(s200.px(0), 0);
        assert_eq!(s200.px(1), 2);
        assert_eq!(s200.px(36), 72);
        assert_eq!(s100.wayland_output_scale(), 1);
        assert_eq!(s150.wayland_output_scale(), 2);
        assert_eq!(s200.wayland_output_scale(), 2);
    }

    #[test]
    fn default_scale_is_200_percent() {
        assert_eq!(UiScale::default_scale().percent(), 200);
        assert_eq!(UiScale::default_scale().wayland_output_scale(), 2);
    }
}
