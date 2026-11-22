// SPDX-License-Identifier: MIT

// Chrome pixel evidence from the guest compositor.
//
// The DVM scans out through virgl dmabufs, so QEMU's `screendump` has no
// surface (`Error: no surface`). The pixels of a Wayland client are instead
// reported by `uiserver` itself (`uiserver: pixel-probe ...`, enabled by the
// private acceptance contract key `ui_pixel_probe=1`). This module judges
// those lines. It proves the client's *committed buffer* has real content; it
// does not prove what the host display shows after virgl composition.

/// Smallest window area (pixels) that can be a browser window rather than a
/// popup or cursor surface.
const CHROME_MIN_WINDOW_AREA: u64 = 320 * 240;
/// A browser window with a tab strip, toolbar and page has far more colors;
/// a gradient-free error or splash frame has very few.
const CHROME_MIN_DISTINCT_COLORS: u64 = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
struct PixelProbeLine {
    surface: u32,
    title: String,
    width: u64,
    height: u64,
    non_black: u64,
    distinct: u64,
    hash: String,
}

fn parse_pixel_probe_line(line: &str) -> Option<PixelProbeLine> {
    // The debugcon transport escapes `"` as `\"` inside `user-debug payload=`;
    // uiserver already replaces quotes and backslashes in the title itself.
    let line = line.replace("\\\"", "\"");
    let rest = line.split_once("uiserver: pixel-probe ")?.1;
    let rest = rest.strip_prefix("surface=")?;
    let (surface, rest) = rest.split_once(" title=\"")?;
    let (title, rest) = rest.split_once("\" size=")?;
    let (size, rest) = rest.split_once(" non_black=")?;
    let (width, height) = size.split_once('x')?;
    let (non_black, rest) = rest.split_once(" distinct=")?;
    let (distinct, rest) = rest.split_once(" hash=")?;
    let hash: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    Some(PixelProbeLine {
        surface: surface.parse().ok()?,
        title: title.to_owned(),
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        non_black: non_black.parse().ok()?,
        // `+` marks a saturated count; it is still "at least this many".
        distinct: distinct.trim_end_matches('+').parse().ok()?,
        hash: (hash.len() == 16).then_some(hash)?,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChromePixelVerdict {
    surface: u32,
    title: String,
    width: u64,
    height: u64,
    non_black: u64,
    distinct: u64,
    frames_seen: usize,
    distinct_hashes: usize,
}

/// Judge the `pixel-probe` lines in a (rejoined) debugcon log. Passes when one
/// Chrome-titled window of browser size reports non-uniform, non-blank content.
fn verify_chrome_pixels(log: &str) -> Result<ChromePixelVerdict> {
    let lines: Vec<PixelProbeLine> = log.lines().filter_map(parse_pixel_probe_line).collect();
    if lines.is_empty() {
        bail!(
            "no `uiserver: pixel-probe` line reached the log: the probe is off (contract key \
             ui_pixel_probe), no Wayland window committed, or the log lost the lines"
        );
    }
    let is_chrome = |line: &&PixelProbeLine| line.title.to_ascii_lowercase().contains("chrom");
    let qualifying = |line: &&PixelProbeLine| {
        line.width.saturating_mul(line.height) >= CHROME_MIN_WINDOW_AREA
            && line.non_black > 0
            && line.distinct >= CHROME_MIN_DISTINCT_COLORS
    };
    // The best (most colorful) qualifying report of one surface decides.
    let best = lines
        .iter()
        .filter(is_chrome)
        .filter(qualifying)
        .max_by_key(|line| line.distinct);
    let Some(best) = best else {
        let mut seen: Vec<String> = Vec::new();
        for line in &lines {
            let entry = format!(
                "surface={} title={:?} {}x{} non_black={} distinct={}",
                line.surface, line.title, line.width, line.height, line.non_black, line.distinct
            );
            if !seen.contains(&entry) && seen.len() < 12 {
                seen.push(entry);
            }
        }
        bail!(
            "no Chrome window with real pixels (title contains \"chrom\", >= {CHROME_MIN_WINDOW_AREA} px, \
             non-black, >= {CHROME_MIN_DISTINCT_COLORS} colors); windows seen: [{}]",
            seen.join("; ")
        );
    };
    let same_surface: Vec<&PixelProbeLine> =
        lines.iter().filter(|line| line.surface == best.surface).collect();
    let mut hashes: Vec<&str> = same_surface.iter().map(|line| line.hash.as_str()).collect();
    hashes.sort_unstable();
    hashes.dedup();
    Ok(ChromePixelVerdict {
        surface: best.surface,
        title: best.title.clone(),
        width: best.width,
        height: best.height,
        non_black: best.non_black,
        distinct: best.distinct,
        frames_seen: same_surface.len(),
        distinct_hashes: hashes.len(),
    })
}

#[cfg(test)]
mod pixel_evidence_tests {
    use super::*;

    fn line(surface: u32, title: &str, w: u64, h: u64, non_black: u64, distinct: &str, hash: &str) -> String {
        format!(
            "user-debug payload=[INFO ] ts=1 service=uiserver cat=service lvl=info uiserver: pixel-probe \
             surface={surface} title=\"{title}\" size={w}x{h} non_black={non_black} distinct={distinct} hash={hash}\\n"
        )
    }

    #[test]
    fn a_probe_line_round_trips_including_saturated_color_counts() {
        let text = line(7, "about:blank - Chromium", 1280, 720, 900_000, "4096+", "0123456789abcdef");
        let parsed = parse_pixel_probe_line(&text).expect("parse");
        assert_eq!(parsed.surface, 7);
        assert_eq!(parsed.title, "about:blank - Chromium");
        assert_eq!((parsed.width, parsed.height, parsed.distinct), (1280, 720, 4096));
        assert!(parse_pixel_probe_line("uiserver: pixel-probe surface=x").is_none());
        assert!(parse_pixel_probe_line("unrelated").is_none());
    }

    #[test]
    fn the_real_debugcon_escaping_of_quotes_is_accepted() {
        let raw = "user-debug payload=[INFO ] ts=1795327581837 service=uiserver cat=service lvl=info uiserver: pixel-probe surface=3 title=\\\"Google Chrome and ChromeOS Additional Terms of Service\\\" size=1584x1080 non_black=1710720 distinct=414 hash=e501591baa74df68\\n";
        let verdict = verify_chrome_pixels(raw).expect("real log line");
        assert_eq!((verdict.surface, verdict.width, verdict.height, verdict.distinct), (3, 1584, 1080, 414));
        assert!(verdict.title.contains("Google Chrome"));
    }

    #[test]
    fn chrome_with_a_colorful_large_window_passes_and_reports_frames() {
        let log = [
            line(1, "wayclick", 400, 300, 1000, "9", "aaaaaaaaaaaaaaaa"),
            line(5, "about:blank - Chromium", 1280, 720, 5000, "3", "bbbbbbbbbbbbbbbb"),
            line(5, "about:blank - Chromium", 1280, 720, 800_000, "52", "cccccccccccccccc"),
            line(5, "about:blank - Chromium", 1280, 720, 800_000, "52", "cccccccccccccccc"),
        ]
        .concat();
        let verdict = verify_chrome_pixels(&log.replace("\\n", "\n")).expect("verdict");
        assert_eq!((verdict.surface, verdict.distinct, verdict.frames_seen), (5, 52, 3));
        assert_eq!(verdict.distinct_hashes, 2);
    }

    #[test]
    fn blank_small_or_foreign_windows_do_not_pass_and_are_listed() {
        let log = [
            line(1, "wayclick", 1920, 1080, 5_000_000, "300", "aaaaaaaaaaaaaaaa"),
            line(5, "Chromium", 1280, 720, 0, "1", "bbbbbbbbbbbbbbbb"),
            line(6, "Chromium", 100, 100, 9000, "40", "cccccccccccccccc"),
        ]
        .concat()
        .replace("\\n", "\n");
        let error = verify_chrome_pixels(&log).unwrap_err().to_string();
        assert!(error.contains("no Chrome window with real pixels"), "{error}");
        assert!(error.contains("wayclick") && error.contains("100x100"), "{error}");
        assert!(verify_chrome_pixels("nothing here\n").unwrap_err().to_string().contains("no `uiserver: pixel-probe`"));
    }
}
