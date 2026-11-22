// SPDX-License-Identifier: MIT
//
// Bounded QEMU HMP framebuffer capture and PPM pixel verification for Linux-DVM
// (`virtio-gpu-gl-pci`) KVM smoke tests.

#[derive(Debug, Clone, Eq, PartialEq)]
struct PpmImage {
    width: u32,
    height: u32,
    /// Tightly packed RGB24 pixels (`width * height * 3` bytes).
    pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct ScreendumpStats {
    width: u32,
    height: u32,
    non_black_pixels: usize,
    distinct_colors: usize,
}

fn read_until_hmp_prompt(
    stream: &mut std::os::unix::net::UnixStream,
    deadline: Instant,
) -> Result<String> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0_u8; 256];
    loop {
        let now = Instant::now();
        if now >= deadline {
            bail!(
                "timed out waiting for QEMU HMP prompt (got {:?})",
                String::from_utf8_lossy(&buf)
            );
        }
        stream.set_read_timeout(Some(deadline.saturating_duration_since(now)))?;
        let n = match stream.read(&mut byte) {
            Ok(0) => {
                bail!(
                    "QEMU HMP monitor closed before prompt (got {:?})",
                    String::from_utf8_lossy(&buf)
                );
            }
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        };
        if buf.len() + n > 64 * 1024 {
            bail!("QEMU HMP response exceeded 64 KiB");
        }
        buf.extend_from_slice(&byte[..n]);
        if buf.windows(7).any(|w| w == b"(qemu) ") {
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}

/// `-device virtio-gpu-gl-pci,id=...` of the DVM (see `options.rs`).
const DVM_GPU_DEVICE_ID: &str = "dvm-virtio-gpu";

/// First line of an HMP reply that is an error report, after dropping the
/// terminal control sequences the monitor's line editor interleaves.
fn hmp_error_line(reply: &str) -> Option<String> {
    let mut clean = String::with_capacity(reply.len());
    let mut chars = reply.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for t in chars.by_ref() {
                    if ('@'..='~').contains(&t) {
                        break;
                    }
                }
            }
        } else {
            clean.push(c);
        }
    }
    clean
        .split(['\r', '\n'])
        .map(str::trim)
        .find(|line| line.starts_with("Error"))
        .map(str::to_owned)
}

fn capture_dvm_screendump(
    monitor_socket: &Path,
    output_ppm: &Path,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    if let Some(parent) = output_ppm.parent() {
        fs::create_dir_all(parent)?;
    }
    let abs_output = if output_ppm.is_absolute() {
        output_ppm.to_path_buf()
    } else {
        std::env::current_dir()?.join(output_ppm)
    };
    let output_str = abs_output
        .to_str()
        .context("screendump path must be valid UTF-8")?;
    if output_str.contains(['\n', '\r']) {
        bail!("screendump path contains newline characters");
    }
    // QEMU overwrites an existing file only on success; a stale frame from an
    // earlier run must never satisfy the verification.
    match fs::remove_file(&abs_output) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("remove stale {}", abs_output.display()));
        }
    }
    let mut stream = loop {
        match std::os::unix::net::UnixStream::connect(monitor_socket) {
            Ok(s) => break s,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("connect DVM HMP monitor {}", monitor_socket.display())
                });
            }
        }
    };
    let _banner = read_until_hmp_prompt(&mut stream, deadline)?;
    stream.set_write_timeout(Some(deadline.saturating_duration_since(Instant::now())))?;
    // `screendump filename [-f format] [device [head]]`: without a device QEMU
    // picks the default console, which has no surface under egl-headless.
    let cmd = format!("screendump {output_str} -f ppm {DVM_GPU_DEVICE_ID} 0\n");
    stream.write_all(cmd.as_bytes())?;
    stream.flush()?;
    let reply = read_until_hmp_prompt(&mut stream, deadline)?;
    // The monitor's line editor echoes the command character by character with
    // cursor-control sequences, so the echo is neither contiguous nor free of
    // path words like "error". HMP reports failure as a line `Error: ...`.
    if let Some(line) = hmp_error_line(&reply) {
        bail!("QEMU screendump failed: {line}");
    }
    let meta = fs::metadata(&abs_output)
        .with_context(|| format!("missing screendump output {}", abs_output.display()))?;
    if meta.len() == 0 {
        bail!("QEMU screendump {} is empty", abs_output.display());
    }
    Ok(())
}

fn next_ppm_token<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a str> {
    while *pos < bytes.len() {
        let b = bytes[*pos];
        if b.is_ascii_whitespace() {
            *pos += 1;
        } else if b == b'#' {
            while *pos < bytes.len() && bytes[*pos] != b'\n' {
                *pos += 1;
            }
        } else {
            break;
        }
    }
    let start = *pos;
    while *pos < bytes.len() && !bytes[*pos].is_ascii_whitespace() && bytes[*pos] != b'#' {
        *pos += 1;
    }
    if *pos == start {
        return None;
    }
    std::str::from_utf8(&bytes[start..*pos]).ok()
}

fn parse_ppm_p6(bytes: &[u8]) -> Result<PpmImage> {
    const MAX_DIM: u32 = 16_384;
    let mut pos = 0;
    let magic = next_ppm_token(bytes, &mut pos).context("missing PPM magic header")?;
    if magic != "P6" {
        bail!("unsupported PPM magic `{magic}` (expected P6)");
    }
    let width: u32 = next_ppm_token(bytes, &mut pos)
        .context("missing PPM width")?
        .parse()
        .context("invalid PPM width")?;
    let height: u32 = next_ppm_token(bytes, &mut pos)
        .context("missing PPM height")?
        .parse()
        .context("invalid PPM height")?;
    let maxval: u32 = next_ppm_token(bytes, &mut pos)
        .context("missing PPM maxval")?
        .parse()
        .context("invalid PPM maxval")?;
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        bail!("out-of-range PPM geometry {width}x{height}");
    }
    if maxval != 255 {
        bail!("unsupported PPM maxval {maxval} (expected 255)");
    }
    if pos >= bytes.len() || !bytes[pos].is_ascii_whitespace() {
        bail!("missing single whitespace delimiter before PPM raster");
    }
    pos += 1;
    let expected = usize::try_from(width)?
        .checked_mul(usize::try_from(height)?)
        .and_then(|px| px.checked_mul(3))
        .context("PPM raster byte count overflow")?;
    let raster = bytes
        .get(pos..)
        .context("PPM ended before pixel raster")?;
    if raster.len() != expected {
        bail!(
            "PPM raster length {} does not match {}x{} RGB24 ({expected} bytes)",
            raster.len(),
            width,
            height
        );
    }
    Ok(PpmImage {
        width,
        height,
        pixels: raster.to_vec(),
    })
}

fn inspect_ppm_image(image: &PpmImage) -> ScreendumpStats {
    let mut non_black_pixels = 0_usize;
    let mut colors = BTreeSet::new();
    // Every pixel is inspected: a mostly uniform desktop with a small window
    // would otherwise be misread as a solid frame by sparse sampling.
    let mut previous = [0_u8; 3];
    let mut have_previous = false;
    for rgb in image.pixels.chunks_exact(3) {
        let rgb = [rgb[0], rgb[1], rgb[2]];
        if rgb != [0, 0, 0] {
            non_black_pixels += 1;
        }
        if (!have_previous || rgb != previous) && colors.len() < 256 {
            colors.insert(rgb);
        }
        previous = rgb;
        have_previous = true;
    }
    ScreendumpStats {
        width: image.width,
        height: image.height,
        non_black_pixels,
        distinct_colors: colors.len(),
    }
}

fn verify_ppm_non_blank(image: &PpmImage) -> Result<ScreendumpStats> {
    let stats = inspect_ppm_image(image);
    if stats.non_black_pixels == 0 {
        bail!(
            "screendump {}x{} is completely black",
            stats.width,
            stats.height
        );
    }
    if stats.distinct_colors < 2 {
        bail!(
            "screendump {}x{} rendered only {} distinct color(s)",
            stats.width,
            stats.height,
            stats.distinct_colors
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod screendump_tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn parses_p6_ppm_with_comments_and_verifies_pixels() {
        let mut ppm = b"P6\n# captured by QEMU\n4 2\n255\n".to_vec();
        ppm.extend_from_slice(&[
            10, 20, 30, 40, 50, 60, 0, 0, 0, 255, 255, 255, // row 0
            10, 20, 30, 70, 80, 90, 0, 0, 0, 250, 240, 230, // row 1
        ]);
        let img = parse_ppm_p6(&ppm).expect("valid P6 PPM");
        assert_eq!((img.width, img.height), (4, 2));
        let stats = verify_ppm_non_blank(&img).expect("non-blank multi-color frame");
        assert_eq!(stats.non_black_pixels, 6);
        assert!(stats.distinct_colors >= 2);
    }

    #[test]
    fn rejects_all_black_and_solid_frames() {
        let black = PpmImage {
            width: 4,
            height: 2,
            pixels: vec![0; 4 * 2 * 3],
        };
        assert!(verify_ppm_non_blank(&black).is_err());

        let solid = PpmImage {
            width: 4,
            height: 2,
            pixels: [12_u8, 34, 56].repeat(8),
        };
        assert!(verify_ppm_non_blank(&solid).is_err());
    }

    #[test]
    fn captures_screendump_from_hmp_socket() {
        let temp = tempfile::tempdir().unwrap();
        let sock = temp.path().join("dvm-monitor.sock");
        let out = temp.path().join("shot.ppm");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.write_all(b"QEMU 9.0 monitor\r\n(qemu) ").unwrap();
            let mut cmd = [0_u8; 512];
            let n = conn.read(&mut cmd).unwrap();
            let line = std::str::from_utf8(&cmd[..n]).unwrap();
            let args = line.trim().strip_prefix("screendump ").unwrap();
            let (target, rest) = args.split_once(" -f ppm ").unwrap();
            assert_eq!(rest, "dvm-virtio-gpu 0");
            let mut ppm = b"P6\n2 1\n255\n".to_vec();
            ppm.extend_from_slice(&[200, 10, 10, 10, 200, 10]);
            fs::write(target, &ppm).unwrap();
            conn.write_all(b"(qemu) ").unwrap();
        });

        capture_dvm_screendump(&sock, &out, Duration::from_secs(2)).unwrap();
        server.join().unwrap();
        let parsed = parse_ppm_p6(&fs::read(&out).unwrap()).unwrap();
        verify_ppm_non_blank(&parsed).unwrap();
    }

    #[test]
    fn small_window_on_uniform_desktop_is_not_a_solid_frame() {
        let mut pixels = [20_u8, 30, 40].repeat(256 * 256);
        pixels[3 * 30_000..3 * 30_000 + 3].copy_from_slice(&[250, 250, 250]);
        let image = PpmImage { width: 256, height: 256, pixels };
        assert_eq!(verify_ppm_non_blank(&image).unwrap().distinct_colors, 2);
    }

    #[test]
    fn hmp_error_is_found_through_line_editor_echo_and_path_words_are_not() {
        let echo = "s\u{1b}[K\u{1b}[Dsc\u{1b}[K screendump /x/errors/a.ppm -f ppm dvm-virtio-gpu 0\r\n";
        assert_eq!(hmp_error_line(echo), None);
        let failed = format!("{echo}Error: no surface\r\n(qemu) ");
        assert_eq!(hmp_error_line(&failed).as_deref(), Some("Error: no surface"));
        assert_eq!(hmp_error_line("(qemu) "), None);
    }

    #[test]
    fn stale_output_is_ignored_and_echoed_path_may_contain_error() {
        let temp = tempfile::tempdir().unwrap();
        let sock = temp.path().join("m.sock");
        let out = temp.path().join("error-shots").join("shot.ppm");
        fs::create_dir_all(out.parent().unwrap()).unwrap();
        fs::write(&out, b"stale").unwrap();
        let listener = UnixListener::bind(&sock).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.write_all(b"(qemu) ").unwrap();
            let mut cmd = [0_u8; 512];
            let n = conn.read(&mut cmd).unwrap();
            // Echo the command (path contains "error") but write no file.
            conn.write_all(&cmd[..n]).unwrap();
            conn.write_all(b"\r\n(qemu) ").unwrap();
        });
        let err = capture_dvm_screendump(&sock, &out, Duration::from_secs(2)).unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("missing screendump output"), "{err:#}");
    }
}
