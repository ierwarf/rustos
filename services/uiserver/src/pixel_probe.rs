//! KVM pixel evidence for Wayland client windows.
//!
//! The DVM scans out through virgl dmabufs, so the host has no display surface
//! to screendump; the one place a client's real pixels exist on the RustOS side
//! is the surface the compositor just applied. When `RUSTOS_UI_PIXEL_PROBE=1`
//! (set by `runtimed`, or by the late contract watcher in `acceptance_profile.rs`,
//! from the private KVM acceptance contract key `ui_pixel_probe=1`), each
//! applied commit may report cheap content statistics for that surface as one
//! debugcon line. xtask judges those lines (`--chrome-web-probe`); nothing here
//! decides pass or fail, and the probe never runs in release images.
//!
//! Volume is bounded: a surface reports at most once per [`MIN_INTERVAL`] and at
//! most [`MAX_LINES_PER_SURFACE`] times, across at most [`MAX_SURFACES`]
//! surfaces, so a continuously committing client cannot flood the log or the
//! UI thread (one scan is O(pixels), run-length skipping keeps UI-like content
//! cheap).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_LINES_PER_SURFACE: u32 = 24;
const MAX_SURFACES: usize = 16;
/// Distinct colors counted before the figure saturates.
const DISTINCT_CAP: usize = 4096;
const TITLE_MAX_CHARS: usize = 64;

/// Set by the late contract watcher when uiserver started before `runtimed`
/// could publish the private acceptance contract (the usual KVM order).
static LATE_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn enable() {
    if !LATE_ENABLED.swap(true, Ordering::AcqRel) {
        crate::sys::diag_line("uiserver: pixel probe enabled");
    }
}

pub(crate) fn enabled() -> bool {
    static FROM_ENV: OnceLock<bool> = OnceLock::new();
    LATE_ENABLED.load(Ordering::Acquire)
        || *FROM_ENV.get_or_init(|| {
            matches!(
                std::env::var("RUSTOS_UI_PIXEL_PROBE").as_deref(),
                Ok("1" | "true" | "TRUE" | "yes" | "YES")
            )
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stats {
    non_black: usize,
    distinct: usize,
    saturated: bool,
    hash: u64,
}

fn scan(pixels: &[u32]) -> Stats {
    let mut non_black = 0usize;
    let mut colors: HashSet<u32> = HashSet::new();
    let mut saturated = false;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut previous: Option<u32> = None;
    for &pixel in pixels {
        // Alpha is forced opaque by the snapshot path; compare color only.
        let rgb = pixel & 0x00ff_ffff;
        if rgb != 0 {
            non_black += 1;
        }
        hash = (hash ^ u64::from(rgb)).wrapping_mul(0x0000_0100_0000_01b3);
        if previous == Some(rgb) {
            continue;
        }
        previous = Some(rgb);
        if !saturated {
            colors.insert(rgb);
            if colors.len() >= DISTINCT_CAP {
                saturated = true;
            }
        }
    }
    Stats {
        non_black,
        distinct: colors.len(),
        saturated,
        hash,
    }
}

/// Window titles are client-controlled text inside a line-oriented log: keep
/// it printable, quote-free and short.
fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .take(TITLE_MAX_CHARS)
        .map(|c| if c.is_control() || c == '"' || c == '\\' { '_' } else { c })
        .collect()
}

struct SurfaceLog {
    last: Instant,
    lines: u32,
}

fn log_state() -> &'static Mutex<HashMap<u32, SurfaceLog>> {
    static STATE: OnceLock<Mutex<HashMap<u32, SurfaceLog>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `true` when this surface may report now; updates the budget.
fn admit(surface_id: u32, now: Instant) -> bool {
    let Ok(mut state) = log_state().lock() else {
        return false;
    };
    let tracked = state.len();
    let entry = match state.get_mut(&surface_id) {
        Some(entry) => entry,
        None => {
            if tracked >= MAX_SURFACES {
                return false;
            }
            // The first sighting is admitted immediately.
            state.insert(
                surface_id,
                SurfaceLog {
                    last: now,
                    lines: 1,
                },
            );
            return true;
        }
    };
    if entry.lines >= MAX_LINES_PER_SURFACE || now.duration_since(entry.last) < MIN_INTERVAL {
        return false;
    }
    entry.last = now;
    entry.lines += 1;
    true
}

/// Report the statistics of `pixels` (a `width` x `height` surface) when the
/// probe is enabled and this surface is within its budget.
pub(crate) fn observe(surface_id: u32, title: &str, pixels: &[u32], width: usize, height: usize) {
    if !enabled() || pixels.is_empty() || width == 0 || height == 0 {
        return;
    }
    if !admit(surface_id, Instant::now()) {
        return;
    }
    let stats = scan(pixels);
    let line = format!(
        "uiserver: pixel-probe surface={surface_id} title=\"{}\" size={width}x{height} non_black={} distinct={}{} hash={:016x}",
        sanitize_title(title),
        stats.non_black,
        stats.distinct,
        if stats.saturated { "+" } else { "" },
        stats.hash,
    );
    crate::sys::debug_line(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_counts_non_black_and_distinct_colors_ignoring_alpha() {
        let pixels = [0xff00_0000, 0x0000_0000, 0xff11_2233, 0x0011_2233, 0xffff_ffff];
        let stats = scan(&pixels);
        assert_eq!(stats.non_black, 3);
        assert_eq!(stats.distinct, 3);
        assert!(!stats.saturated);
    }

    #[test]
    fn a_solid_frame_has_one_color_and_equal_frames_hash_equal() {
        let a = vec![0xff20_3040_u32; 1000];
        let b = vec![0x0020_3040_u32; 1000];
        let (sa, sb) = (scan(&a), scan(&b));
        assert_eq!(sa.distinct, 1);
        assert_eq!(sa.hash, sb.hash);
        let mut c = a.clone();
        c[500] = 0xff20_3041;
        assert_ne!(scan(&c).hash, sa.hash);
    }

    #[test]
    fn distinct_colors_saturate_at_the_cap() {
        let pixels: Vec<u32> = (0..(DISTINCT_CAP as u32 * 2)).collect();
        let stats = scan(&pixels);
        assert_eq!(stats.distinct, DISTINCT_CAP);
        assert!(stats.saturated);
    }

    #[test]
    fn titles_are_made_log_safe_and_bounded() {
        assert_eq!(sanitize_title("a\"b\\c\nd"), "a_b_c_d");
        assert_eq!(sanitize_title(&"x".repeat(500)).chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn surfaces_report_once_per_interval_and_within_the_budgets() {
        let t0 = Instant::now();
        let id = 9_000_001;
        assert!(admit(id, t0));
        assert!(!admit(id, t0 + Duration::from_millis(500)));
        assert!(admit(id, t0 + MIN_INTERVAL));
        let mut now = t0 + MIN_INTERVAL;
        let mut admitted = 2;
        for _ in 0..(MAX_LINES_PER_SURFACE * 2) {
            now += MIN_INTERVAL;
            if admit(id, now) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, MAX_LINES_PER_SURFACE);
    }
}
