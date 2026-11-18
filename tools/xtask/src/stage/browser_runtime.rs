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

const HOSTS: &str = "127.0.0.1\tlocalhost\n::1\t\tlocalhost ip6-localhost ip6-loopback\n";
const NSSWITCH: &str = "passwd: files\ngroup: files\nhosts: files dns\n";
// Fallback only: assets/image/etc/resolv.conf normally provides this file.
const RESOLV_CONF: &str = "nameserver 10.0.2.3\noptions timeout:2 attempts:2\n";
const FONT_DIR: &str = "usr/share/fonts/truetype/rustos";
const FONTS_CONF_PATH: &str = "etc/fonts/fonts.conf";
const CA_BUNDLE_PATH: &str = "etc/ssl/certs/ca-certificates.crt";
const HOST_CA_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ca-certificates/extracted/tls-ca-bundle.pem",
    "/etc/pki/tls/certs/ca-bundle.crt",
];
/// Fonts already vendored for uiserver (SIL OFL, licence texts ship with them).
const UI_FONT_SOURCE: &str = "assets/ui/bundle/fonts";

const FONTS_CONF: &str = r#"<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>/usr/share/fonts</dir>
  <cachedir>/data/cache/fontconfig</cachedir>
  <cachedir>/tmp/fontconfig</cachedir>
  <alias><family>sans-serif</family><prefer><family>Inter</family></prefer></alias>
  <alias><family>monospace</family><prefer><family>Maple Mono</family></prefer></alias>
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
    Ok(notes)
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
    fn runtime_env_defaults_put_state_on_data_and_never_use_sandbox_flags() {
        let env: std::collections::BTreeMap<_, _> =
            super::super::DEFAULT_RUNTIME_ENV.iter().copied().collect();
        assert_eq!(env["HOME"], "/data/home");
        assert_eq!(env["XDG_CONFIG_HOME"], "/data/config");
        assert_eq!(env["XDG_CACHE_HOME"], "/data/cache");
        assert_eq!(env["XDG_RUNTIME_DIR"], "/run/user/1000");
        assert_eq!(env["TMPDIR"], "/tmp");
        assert_eq!(env["WAYLAND_DISPLAY"], "wayland-0");
    }

    #[test]
    fn chrome_example_manifest_has_no_sandbox_bypass_and_no_env_override() {
        let text = include_str!("../../../../apps/chrome/RUSTOS.package.toml.example");
        assert!(!text.contains("no-sandbox") && !text.contains("disable-setuid-sandbox"));
        assert!(!text.contains("env ="), "per-entry env would override the shared defaults");
    }
}
