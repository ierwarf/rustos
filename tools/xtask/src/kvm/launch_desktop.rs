// Evidence and contract for `kvm-smoke --launch-desktop <desktop-id>`: the
// private, KVM-only request that runtimed launch one registered desktop entry
// once the UI is ready, through its ordinary launch path. Owner contract:
// `docs/ai/commands.md` (KVM smoke arguments) and `services/runtimed/src/
// kvm_launch_desktop.rs`.
//
// The request is a contract file on the per-run private boot disk. Without it
// runtimed changes no behavior, and the file is never part of a product image.

const PRIVATE_LAUNCH_DESKTOP_CONTRACT_PATH: &str =
    "system/registry/system/kvm-launch-desktop-v1.env";
/// Must equal `CONTRACT_HEADER` in `services/runtimed/src/kvm_launch_desktop.rs`.
const LAUNCH_DESKTOP_CONTRACT_HEADER: &str = "contract=rustos-kvm-launch-desktop-v1";
/// Must equal `MAX_DESKTOP_ID_BYTES` in the runtimed parser.
const LAUNCH_DESKTOP_ID_MAX_BYTES: usize = 64;
const LAUNCH_DESKTOP_SUFFIX: &str = ".desktop";
const LAUNCH_DESKTOP_FAIL_MARKER: &str = "runtimed: kvm launch-desktop FAIL";
const LAUNCH_DESKTOP_LAUNCHED_PREFIX: &str = "runtimed: kvm launch-desktop launched desktop_id=";

/// The registry file-name grammar `package_manifest` enforces for package ids
/// (`[A-Za-z0-9_-]+`) plus the `-N` entry index and the `.desktop` suffix.
fn validate_launch_desktop_id(id: &str) -> Result<()> {
    let Some(stem) = id.strip_suffix(LAUNCH_DESKTOP_SUFFIX) else {
        bail!("--launch-desktop must be a desktop id ending in `.desktop`, got {id:?}");
    };
    if stem.is_empty() {
        bail!("--launch-desktop id has an empty name before `.desktop`: {id:?}");
    }
    if id.len() > LAUNCH_DESKTOP_ID_MAX_BYTES {
        bail!(
            "--launch-desktop id is longer than {LAUNCH_DESKTOP_ID_MAX_BYTES} bytes: {} bytes",
            id.len()
        );
    }
    if let Some(bad) = stem
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
    {
        bail!(
            "--launch-desktop id {id:?} contains {bad:?}; only ASCII letters, digits, '-' and '_' before `.desktop` are allowed (no path separators)"
        );
    }
    Ok(())
}

/// The id must be a file of the staged registry (`usr/share/applications`), the
/// one runtimed's catalog is built from. An unknown id is refused before any
/// guest boots; runtimed re-checks against its own loaded catalog.
fn validate_launch_desktop_registered(applications_dir: &Path, id: &str) -> Result<()> {
    validate_launch_desktop_id(id)?;
    if applications_dir.join(id).is_file() {
        return Ok(());
    }
    let mut known = fs::read_dir(applications_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| name.ends_with(LAUNCH_DESKTOP_SUFFIX))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    known.sort();
    bail!(
        "--launch-desktop {id}: no such desktop entry in the staged registry {} (known: {}); run `cargo xtask build` if the image is stale",
        applications_dir.display(),
        if known.is_empty() {
            String::from("none")
        } else {
            known.join(", ")
        }
    );
}

fn render_launch_desktop_contract(id: &str) -> String {
    format!("{LAUNCH_DESKTOP_CONTRACT_HEADER}\ndesktop_id={id}\n")
}

fn launch_desktop_launched_marker(id: &str) -> String {
    format!("{LAUNCH_DESKTOP_LAUNCHED_PREFIX}{id}")
}

/// runtimed's own refusal record (unknown id, malformed contract), trimmed of
/// the debugcon framing. It never recovers, so it ends the boot at once.
fn launch_desktop_failure(log: &str) -> Option<String> {
    let line = log
        .lines()
        .find(|line| line.contains(LAUNCH_DESKTOP_FAIL_MARKER))?;
    let at = line.find(LAUNCH_DESKTOP_FAIL_MARKER)?;
    Some(line[at..].trim().trim_end_matches("\\n").to_owned())
}

#[cfg(test)]
mod launch_desktop_tests {
    use super::*;

    #[test]
    fn well_formed_ids_are_accepted() {
        for id in ["chrome.desktop", "my-app_2.desktop", "a-1.desktop"] {
            assert!(validate_launch_desktop_id(id).is_ok(), "{id}");
        }
    }

    #[test]
    fn malformed_ids_are_refused() {
        let long = format!("{}.desktop", "a".repeat(LAUNCH_DESKTOP_ID_MAX_BYTES));
        for id in [
            "",
            ".desktop",
            "chrome",
            "chrome.desktop ",
            "../chrome.desktop",
            "a/b.desktop",
            "a\\b.desktop",
            "a.b.desktop",
            "a b.desktop",
            "chrome.desktop\n",
            "chrome.desktop.desktop",
            "chröme.desktop",
            long.as_str(),
        ] {
            assert!(validate_launch_desktop_id(id).is_err(), "{id:?}");
        }
    }

    #[test]
    fn unknown_ids_are_a_hard_error_naming_the_registry() {
        let dir = std::env::temp_dir().join(format!("rustos-launch-desktop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("shell.desktop"), b"[Desktop Entry]\n").unwrap();
        assert!(validate_launch_desktop_registered(&dir, "shell.desktop").is_ok());
        let err = validate_launch_desktop_registered(&dir, "chrome.desktop")
            .unwrap_err()
            .to_string();
        assert!(err.contains("chrome.desktop") && err.contains("shell.desktop"), "{err}");
        assert!(validate_launch_desktop_registered(&dir, "../x.desktop").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn contract_and_markers_are_exact() {
        assert_eq!(
            render_launch_desktop_contract("chrome.desktop"),
            "contract=rustos-kvm-launch-desktop-v1\ndesktop_id=chrome.desktop\n"
        );
        assert_eq!(
            launch_desktop_launched_marker("chrome.desktop"),
            "runtimed: kvm launch-desktop launched desktop_id=chrome.desktop"
        );
        let log = "noise\n[dbg] runtimed: kvm launch-desktop FAIL desktop_id=x.desktop errno=2\\n\n";
        let failure = launch_desktop_failure(log).unwrap();
        assert!(failure.ends_with("errno=2"), "{failure}");
        assert!(launch_desktop_failure("runtimed: kvm launch-desktop launched desktop_id=a").is_none());
    }
}
