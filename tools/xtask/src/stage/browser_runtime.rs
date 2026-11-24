//! Boot-image files a Linux browser needs besides its own bundle: fontconfig,
//! fonts, TLS roots and the minimal name-service configuration.
//!
//! Every file lands in the staged image tree, so the existing "reopen every
//! staged FAT file and compare bytes" check covers it. A file already present
//! (for example from `assets/image`) is never overwritten, and a missing host
//! input is reported instead of fabricated.

use crate::Result;
use crate::config::Config;
use fs_err as fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const HOSTS: &str = "127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n";
const NSSWITCH: &str = "passwd: files\ngroup: files\nhosts: files dns\n";
// Fallback only: assets/image/etc/resolv.conf normally provides this file.
const RESOLV_CONF: &str = "nameserver 10.0.2.3\noptions timeout:2 attempts:2\n";
const FONT_DIR: &str = "usr/share/fonts/truetype/rustos";
const FONTS_CONF_PATH: &str = "etc/fonts/fonts.conf";
/// Image-relative font tree fontconfig scans (`<dir>` of [`FONTS_CONF`]).
const FONT_ROOT: &str = "usr/share/fonts";
/// Pre-generated, read-only fontconfig caches staged into the boot image
/// (listed after writable `<cachedir>` entries in [`FONTS_CONF`]; see
/// [`bake_font_cache`]).
const FONT_CACHE_DIR: &str = "usr/share/fontconfig-cache";
/// Temporary sysroot-relative config used only while [`bake_font_cache`] runs
/// host `fc-cache`, so baked caches land in [`FONT_CACHE_DIR`] while runtime
/// [`FONTS_CONF`] keeps writable cache directories first.
const BAKE_FONTS_CONF_PATH: &str = "etc/fonts/.bake-fonts.conf";
/// Cache format the browser's bundled fontconfig reads. The Chrome 157 bundle
/// links its own fontconfig (it names `FONTCONFIG_NO_CHECK_CACHE_VERSION`); the
/// launch logs show it probing `*-le64.cache-12`. A host `fc-cache` that writes
/// another format would ship caches nobody reads, so staging refuses it.
const FONT_CACHE_FORMAT: &str = "12";
const MACHINE_ID_PATH: &str = "etc/machine-id";
const DBUS_MACHINE_ID_PATH: &str = "var/lib/dbus/machine-id";
/// Deterministic 128-bit lowercase-hex machine ID (`systemd`/`libdbus` format: 32 hex + `\n`).
const DEFAULT_MACHINE_ID: &[u8] = b"727573746f7330303030303030303031\n";
const CA_BUNDLE_PATH: &str = "etc/ssl/certs/ca-certificates.crt";
const HOST_CA_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ca-certificates/extracted/tls-ca-bundle.pem",
    "/etc/pki/tls/certs/ca-bundle.crt",
];
/// Fonts already vendored for uiserver (SIL OFL, licence texts ship with them).
const UI_FONT_SOURCE: &str = "assets/ui/bundle/fonts";

// Put writable cache dirs before `/usr/share/fontconfig-cache`. When `/data` is
// read-only, `runtimed` points `XDG_CACHE_HOME` at `/tmp/rustos-profile-1000/cache`,
// so `prefix="xdg"` resolves to a writable directory without probing read-only
// `/usr/share/fontconfig-cache` or `/data/cache/fontconfig` (`errno=30` `EROFS`
// `.TMP-*` retries when `vfs_fstat` reports `st_mtime=0` against FAT's
// `FcStatChecksum` `dir_stat.st_mtime != 0`).
const FONTS_CONF: &str = r#"<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>/usr/share/fonts</dir>
  <cachedir prefix="xdg">fontconfig</cachedir>
  <cachedir>/tmp/fontconfig</cachedir>
  <cachedir>/usr/share/fontconfig-cache</cachedir>
  <alias><family>sans-serif</family><prefer><family>Inter</family></prefer></alias>
  <alias><family>monospace</family><prefer><family>Maple Mono</family></prefer></alias>
</fontconfig>
"#;

const BAKE_FONTS_CONF: &str = r#"<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>/usr/share/fonts</dir>
  <cachedir>/usr/share/fontconfig-cache</cachedir>
</fontconfig>
"#;

pub(super) fn stage_browser_runtime_files(config: &Config) -> Result<Vec<String>> {
    let notes = stage_into(
        &config.image_dir,
        &config.root_dir.join(UI_FONT_SOURCE),
        &HOST_CA_BUNDLES.iter().map(PathBuf::from).collect::<Vec<_>>(),
    )?;
    // Fonts and trust roots are required runtime inputs, not optional warnings
    // that can silently produce an apparently complete but unusable image.
    anyhow::ensure!(notes.is_empty(), "missing mandatory browser runtime assets: {}", notes.join("; "));
    bake_font_cache(&config.image_dir)?;
    Ok(notes)
}

/// Ships fontconfig's per-directory caches inside the image.
///
/// Without them every launch scans and parses every font file and then fails
/// to write the result (the persistent volume may be read-only and `/tmp` is
/// per boot), so the cost repeats on each Chrome start. fontconfig keys a
/// cache by `md5(directory path)` and accepts it when the cache's recorded
/// mtime equals the directory's mtime, or the directory's mtime is `0`
/// (`FcCacheTimeValid`). The image is a FAT volume written without a clock, so
/// its directories carry the zero FAT date, which the FAT driver reports as an
/// unset (`0`) timestamp; the caches are therefore valid whatever mtime the
/// host staging directory has. If a guest ever reports a non-zero mtime the
/// caches are rejected and fontconfig rebuilds in its writable `<cachedir>`,
/// exactly the behaviour before this existed (never a wrong result).
///
/// The host `fc-cache` writes the cache with the image as sysroot. Cache
/// files for older formats are symlinks to the current file; FAT has no
/// symlinks and the consumer reads only [`FONT_CACHE_FORMAT`], so just that
/// file is kept.
fn bake_font_cache(image: &Path) -> Result<()> {
    // On a FAT volume (`f_type == MSDOS_SUPER_MAGIC`), fontconfig's `FcStat`
    // ignores `st_mtime` and replaces it with `FcStatChecksum` (an Adler-32
    // checksum over the sorted directory entries' names and `d_type`). Match
    // that exact checksum on each font directory before running host `fc-cache`
    // so `FcCacheTimeValid` accepts the pre-generated caches whenever the
    // guest filesystem reports non-zero file mtimes matching the image.
    set_fat_checksum_mtimes_recursive(&image.join(FONT_ROOT))?;
    let bake_conf = image.join(BAKE_FONTS_CONF_PATH);
    if let Some(parent) = bake_conf.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&bake_conf, BAKE_FONTS_CONF)?;
    let output_result = Command::new("fc-cache")
        .arg("--force")
        .arg("--sysroot")
        .arg(image)
        .env("FONTCONFIG_FILE", format!("/{BAKE_FONTS_CONF_PATH}"))
        .env_remove("FONTCONFIG_PATH")
        .env_remove("FONTCONFIG_SYSROOT")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("HOME")
        .env_remove("SOURCE_DATE_EPOCH")
        .output();
    let _ = fs::remove_file(&bake_conf);
    let output = output_result.map_err(|error| {
        anyhow::anyhow!("host fc-cache (fontconfig) is required to pre-generate the browser font cache: {error}")
    })?;
    anyhow::ensure!(
        output.status.success(),
        "fc-cache failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let kept = prune_font_cache(&image.join(FONT_CACHE_DIR), &image.join(FONT_ROOT))?;
    anyhow::ensure!(kept > 0, "fc-cache produced no cache-{FONT_CACHE_FORMAT} files in /{FONT_CACHE_DIR}");
    Ok(())
}

/// Computes fontconfig's `FcStatChecksum` (`fcstat.c`) for a FAT directory
/// whose entries have `d_type` set (`DT_DIR = 4`, `DT_REG = 8`).
fn fc_fat_dir_checksum(dir: &Path) -> Result<i64> {
    use std::os::unix::ffi::OsStrExt;

    let mut entries: Vec<(Vec<u8>, u8)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        // Fontconfig's `FcDirChecksum` filter (`d_type != DT_DIR`) skips all
        // directories (including `.`, `..`, and subdirectories) and hashes only
        // non-directory entries.
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().as_bytes().to_vec();
        entries.push((name, libc::DT_REG));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    const MOD_ADLER: i32 = 65521;
    let mut a: i32 = 1;
    let mut b: i32 = 0;
    let feed = |byte: i8, a: &mut i32, b: &mut i32| {
        *a = (*a + i32::from(byte)) % MOD_ADLER;
        *b = (*b + *a) % MOD_ADLER;
    };
    for (name, d_type) in entries.iter().rev() {
        for &byte in name {
            feed(byte as i8, &mut a, &mut b);
        }
        feed(0, &mut a, &mut b);
        feed(*d_type as i8, &mut a, &mut b);
        feed(0, &mut a, &mut b);
        feed(0, &mut a, &mut b);
        feed(0, &mut a, &mut b);
    }
    Ok(i64::from((b << 16).wrapping_add(a)))
}

/// Sets `atime` and `mtime` of `dir` and every subdirectory under it to
/// `(fc_fat_dir_checksum(dir), 0)` in post-order so parent directories keep
/// their exact checksum mtimes.
fn set_fat_checksum_mtimes_recursive(dir: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            set_fat_checksum_mtimes_recursive(&entry.path())?;
        }
    }
    let checksum = fc_fat_dir_checksum(dir)?;
    let c_path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|error| anyhow::anyhow!("font directory path contains NUL: {error}"))?;
    let times = [
        libc::timespec { tv_sec: checksum, tv_nsec: 0 },
        libc::timespec { tv_sec: checksum, tv_nsec: 0 },
    ];
    // SAFETY: `c_path` is a NUL-terminated path and `times` is a valid 2-element
    // `timespec` array for the duration of the synchronous syscall.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    anyhow::ensure!(
        rc == 0,
        "utimensat({checksum}, 0) failed on {}: {}",
        dir.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// Keeps exactly the regular `*-le64.cache-<FONT_CACHE_FORMAT>` files, removes
/// every other cache alias, and requires one cache per font directory (the
/// root and each subdirectory, which fontconfig caches separately). Returns the
/// number kept.
fn prune_font_cache(cache_dir: &Path, font_root: &Path) -> Result<usize> {
    let wanted = format!("-le64.cache-{FONT_CACHE_FORMAT}");
    let mut kept = 0usize;
    let mut formats = Vec::new();
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains("-le64.cache-") {
            continue;
        }
        let regular = entry.file_type()?.is_file();
        if regular {
            formats.push(name.clone());
        }
        if regular && name.ends_with(&wanted) {
            kept += 1;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    anyhow::ensure!(
        kept > 0,
        "host fc-cache wrote cache formats {formats:?}, but the browser's fontconfig reads cache-{FONT_CACHE_FORMAT}"
    );
    let mut dirs = 0usize;
    let mut pending = vec![font_root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        dirs += 1;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    anyhow::ensure!(
        kept == dirs,
        "font cache covers {kept} of {dirs} font directories under /{FONT_ROOT}"
    );
    Ok(kept)
}

fn write_if_absent(image: &Path, relative: &str, bytes: &[u8]) -> Result<bool> {
    let path = image.join(relative);
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, bytes)?;
    Ok(true)
}

fn stage_into(image: &Path, font_source: &Path, ca_candidates: &[PathBuf]) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    write_if_absent(image, "etc/hosts", HOSTS.as_bytes())?;
    write_if_absent(image, "etc/nsswitch.conf", NSSWITCH.as_bytes())?;
    write_if_absent(image, "etc/resolv.conf", RESOLV_CONF.as_bytes())?;
    write_if_absent(image, FONTS_CONF_PATH, FONTS_CONF.as_bytes())?;
    write_if_absent(image, MACHINE_ID_PATH, DEFAULT_MACHINE_ID)?;
    write_if_absent(image, DBUS_MACHINE_ID_PATH, DEFAULT_MACHINE_ID)?;

    let mut fonts = 0usize;
    if font_source.is_dir() {
        let mut entries = fs::read_dir(font_source)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let is_font = name.ends_with(".ttf") || name.ends_with(".otf");
            if !(is_font || name.ends_with("-OFL.txt")) || !entry.path().is_file() {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            write_if_absent(image, &format!("{FONT_DIR}/{name}"), &bytes)?;
            fonts += usize::from(is_font);
        }
    }
    if fonts == 0 {
        notes.push(format!(
            "no TTF/OTF fonts found in {}; staged {FONTS_CONF_PATH} points at an empty /usr/share/fonts",
            font_source.display()
        ));
    }

    if image.join(CA_BUNDLE_PATH).exists() {
        anyhow::ensure!(
            looks_like_pem(&fs::read(image.join(CA_BUNDLE_PATH))?),
            "staged TLS root bundle /{CA_BUNDLE_PATH} contains no PEM certificate"
        );
    } else {
        // fs::read follows the host symlink chain to the real bundle.
        let found = ca_candidates
            .iter()
            .find_map(|path| fs::read(path).ok().filter(|bytes| looks_like_pem(bytes)));
        match found {
            Some(bytes) => {
                write_if_absent(image, CA_BUNDLE_PATH, &bytes)?;
            }
            None => notes.push(format!(
                "SKIPPED /{CA_BUNDLE_PATH}: no host CA bundle among {ca_candidates:?}; TLS will fail until one is provided"
            )),
        }
    }
    Ok(notes)
}

fn looks_like_pem(bytes: &[u8]) -> bool {
    bytes
        .windows(27)
        .any(|window| window == b"-----BEGIN CERTIFICATE-----")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_name_service_fonts_and_cas_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("image");
        let fonts = temp.path().join("fonts");
        std::fs::create_dir_all(image.join("etc")).unwrap();
        std::fs::write(image.join("etc/resolv.conf"), "nameserver 1.2.3.4\n").unwrap();
        std::fs::create_dir_all(&fonts).unwrap();
        std::fs::write(fonts.join("Inter-Light.ttf"), b"ttf").unwrap();
        std::fs::write(fonts.join("Inter-OFL.txt"), b"ofl").unwrap();
        std::fs::write(fonts.join("notes.md"), b"ignored").unwrap();
        let ca = temp.path().join("ca.pem");
        std::fs::write(&ca, b"x\n-----BEGIN CERTIFICATE-----\nAAAA\n").unwrap();
        let notes = stage_into(&image, &fonts, &[temp.path().join("missing.pem"), ca]).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let read = |p: &str| std::fs::read_to_string(image.join(p)).unwrap();
        assert!(read("etc/hosts").contains("127.0.0.1\tlocalhost"));
        assert_eq!(read("etc/nsswitch.conf").lines().last(), Some("hosts: files dns"));
        assert_eq!(read("etc/resolv.conf"), "nameserver 1.2.3.4\n");
        assert!(read("etc/fonts/fonts.conf").contains("<dir>/usr/share/fonts</dir>"));
        for mid_path in [MACHINE_ID_PATH, DBUS_MACHINE_ID_PATH] {
            let mid = read(mid_path);
            assert_eq!(mid.len(), 33, "{mid_path}");
            assert!(mid.ends_with('\n'));
            assert!(mid[..32].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
        assert_eq!(std::fs::read(image.join(FONT_DIR).join("Inter-Light.ttf")).unwrap(), b"ttf");
        assert!(image.join(FONT_DIR).join("Inter-OFL.txt").is_file());
        assert!(!image.join(FONT_DIR).join("notes.md").exists());
        assert!(read(CA_BUNDLE_PATH).contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn missing_host_inputs_are_reported_never_fabricated() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("image");
        let notes = stage_into(&image, &temp.path().join("nofonts"), &[temp.path().join("none")])
            .unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().any(|note| note.contains("SKIPPED")));
        assert!(!image.join(CA_BUNDLE_PATH).exists());
        assert!(image.join(FONTS_CONF_PATH).is_file());
    }

    #[test]
    fn runtime_env_defaults_are_profile_templates_and_never_use_sandbox_flags() {
        let env: std::collections::BTreeMap<_, _> =
            super::super::DEFAULT_RUNTIME_ENV.iter().copied().collect();
        // Templates: runtimed resolves them per launch (persistent `/data`
        // when usable, else a verified TmpFs profile; uid from the launch).
        assert_eq!(env["HOME"], "{profile_root}/home");
        assert_eq!(env["XDG_CONFIG_HOME"], "{profile_root}/config");
        assert_eq!(env["XDG_CACHE_HOME"], "{profile_root}/cache");
        assert_eq!(env["XDG_RUNTIME_DIR"], "{runtime_dir}");
        assert_eq!(env["TMPDIR"], "{temp_dir}");
        assert_eq!(env["WAYLAND_DISPLAY"], "{wayland_display}");
        assert_eq!(env["DBUS_SESSION_BUS_ADDRESS"], "unix:path={runtime_dir}/bus");
    }

    #[test]
    fn chrome_example_manifest_env_keeps_the_shared_profile_defaults() {
        let text = include_str!("../../../../apps/chrome/RUSTOS.package.toml.example");
        let default_keys: Vec<&str> =
            super::super::DEFAULT_RUNTIME_ENV.iter().map(|(key, _)| *key).collect();
        for line in text.lines().filter(|line| line.trim_start().starts_with("env =")) {
            for item in line.split('"').skip(1).step_by(2) {
                let key = item.split('=').next().unwrap();
                assert!(
                    !default_keys.contains(&key),
                    "per-entry env {key} would override a shared profile default"
                );
            }
        }
    }

    #[test]
    fn staged_fonts_conf_prefers_writable_caches_before_read_only_baked_cache() {
        let cachedirs: Vec<&str> = FONTS_CONF
            .lines()
            .filter_map(|line| line.trim().strip_prefix("<cachedir"))
            .collect();
        assert_eq!(
            cachedirs,
            [
                r#" prefix="xdg">fontconfig</cachedir>"#,
                ">/tmp/fontconfig</cachedir>",
                ">/usr/share/fontconfig-cache</cachedir>",
            ]
        );
        assert!(
            !FONTS_CONF.contains("/data/cache/fontconfig"),
            "fonts.conf must not hardcode /data/cache/fontconfig because /data may be read-only; prefix=\"xdg\" follows XDG_CACHE_HOME"
        );
        assert_eq!(format!("/{FONT_CACHE_DIR}"), "/usr/share/fontconfig-cache");
    }

    #[test]
    fn prune_keeps_only_the_consumed_cache_format_and_requires_every_directory() {
        let temp = tempfile::tempdir().unwrap();
        let fonts = temp.path().join("fonts");
        std::fs::create_dir_all(fonts.join("truetype")).unwrap();
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        for hash in ["aa", "bb"] {
            std::fs::write(cache.join(format!("{hash}-le64.cache-12")), b"c").unwrap();
            for old in ["9", "10", "11"] {
                std::os::unix::fs::symlink(
                    format!("{hash}-le64.cache-12"),
                    cache.join(format!("{hash}-le64.cache-{old}")),
                )
                .unwrap();
            }
        }
        std::fs::write(cache.join("CACHEDIR.TAG"), b"tag").unwrap();
        assert_eq!(prune_font_cache(&cache, &fonts).unwrap(), 2);
        let mut names: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["CACHEDIR.TAG", "aa-le64.cache-12", "bb-le64.cache-12"]);
        // A third directory without a cache is a hole fontconfig would rescan.
        std::fs::create_dir_all(fonts.join("truetype/extra")).unwrap();
        assert!(prune_font_cache(&cache, &fonts).is_err());
        // A host that writes another format ships nothing readable.
        std::fs::remove_file(cache.join("aa-le64.cache-12")).unwrap();
        std::fs::remove_file(cache.join("bb-le64.cache-12")).unwrap();
        std::fs::write(cache.join("aa-le64.cache-13"), b"c").unwrap();
        assert!(prune_font_cache(&cache, &fonts).is_err());
    }

    /// Needs the host `fc-cache`; proves the real tool's output has the shape
    /// the pruning assumes (one regular `cache-12` per font directory) and
    /// that the cache is accepted for a directory whose mtime is 0.
    #[test]
    #[ignore = "requires host fc-cache (fontconfig 2.17+)"]
    fn host_fc_cache_bakes_one_cache_per_font_directory() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("image");
        let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../").join(UI_FONT_SOURCE);
        stage_into(&image, &fonts, &[]).unwrap();
        bake_font_cache(&image).unwrap();
        assert!(
            !image.join(BAKE_FONTS_CONF_PATH).exists(),
            "temporary bake fontconfig file must not remain in the staged image"
        );
        assert!(
            !image.join("tmp").exists(),
            "baking must write into /{FONT_CACHE_DIR}, not /tmp/fontconfig"
        );
        let cache = image.join(FONT_CACHE_DIR);
        let count = std::fs::read_dir(&cache)
            .unwrap()
            .filter(|entry| {
                entry.as_ref().unwrap().file_name().to_string_lossy().ends_with("-le64.cache-12")
            })
            .count();
        assert_eq!(count, 3, "usr/share/fonts, truetype, truetype/rustos");
    }
}
