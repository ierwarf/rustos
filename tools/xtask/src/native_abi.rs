//! Native ABI revision stamps: build-time enforcement.
//!
//! A native binary is a packaged executable (`builder = cargo-kernel-binary`)
//! whose crate depends directly on `rustos-user-abi` or `rustos-svc-runtime`,
//! i.e. one that can use the native `0x5255_xxxx` syscall/IPC surface. This
//! covers the static-PIE boot services (rootd, syscalld, vfsd, loaderd, procd,
//! pagerd, spawned by the kernel bootstrap allowlist without `loaderd`) and the
//! dynamic Rust-std services and apps (devmgrd, initd, inputd, netd, runtimed,
//! storaged, uiserver, ipcbench, wayclick). Binaries that do not link the ABI
//! crates (C demos, apps/shell, sessiond, prebuilt apps) are ordinary Linux
//! ELFs and stay unstamped. A new binary is covered automatically by the
//! dependency rule; the only way out is an entry in `NOT_STAMPED` below.
//!
//! Each native binary must carry the `.note.rustos.abi` note emitted by
//! `rustos_user_abi::native_abi_note!` (or `rustos_svc_runtime::entry!`) for
//! `NATIVE_ABI_REVISION_CURRENT`.
//!
//! * `check_native_abi_stamps` (run by `cargo xtask check` and `build`): every
//!   native package's sources expand the macro, and every already-built
//!   artifact newer than the stamp sources carries the current revision.
//! * `verify_staged_native_elfs` (run by `stage`): every staged native ELF is
//!   stamped, using the same `admit_native_abi_stamp` code `loaderd` runs. The
//!   kernel does not parse notes, so this is what covers boot-spawned services.
//!
//! The stamp is a self-declared compatibility hint, not authority.

use anyhow::{Context, bail};
use fs_err as fs;
use rustos_image_admission::{
    ELF64_HEADER_SIZE, ELF64_PROGRAM_HEADER_SIZE, NativeAbiRefusal, admit_native_abi_stamp,
};
use rustos_user_abi::abi_revision::NATIVE_ABI_REVISION_CURRENT;
use std::path::Path;
use walkdir::WalkDir;

use crate::Result;
use crate::config::Config;
use crate::package_manifest::{BuilderKind, PackageManifest, load_manifests};

/// Source files whose edit makes older artifacts irrelevant to the check.
const STAMP_SOURCES: &[&str] = &[
    "libs/rustos-user-abi/src/abi_revision.rs",
    "libs/rustos-svc-runtime/src/lib.rs",
];

/// Packages that depend on an ABI crate but are deliberately not stamped,
/// each with its reason. Empty today; an entry needs a reviewed justification.
const NOT_STAMPED: &[(&str, &str)] = &[];

const ABI_CRATES: &[&str] = &["rustos-user-abi", "rustos-svc-runtime"];

/// True when the package's `[dependencies]` names an ABI crate directly.
fn depends_on_abi_crate(package_root: &Path) -> bool {
    let Ok(text) = fs::read_to_string(package_root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return false;
    };
    value
        .get("dependencies")
        .and_then(|d| d.as_table())
        .is_some_and(|deps| ABI_CRATES.iter().any(|name| deps.contains_key(*name)))
}

pub(crate) fn is_native_package(manifest: &PackageManifest) -> bool {
    manifest.build.builder == BuilderKind::CargoKernelBinary
        && depends_on_abi_crate(&manifest.package_root)
        && !NOT_STAMPED.iter().any(|(id, _)| *id == manifest.id)
}

/// Verifies the stamp of one ELF image given as bytes.
pub(crate) fn verify_native_elf_bytes(bytes: &[u8]) -> std::result::Result<u32, String> {
    if bytes.len() < ELF64_HEADER_SIZE || bytes[..4] != *b"\x7fELF" || bytes[4] != 2 {
        return Err("not an ELF64 image".into());
    }
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]) as usize;
    let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let phnum = u16_at(56);
    if u16_at(54) != ELF64_PROGRAM_HEADER_SIZE {
        return Err("unexpected program header size".into());
    }
    let table = phoff
        .checked_add(phnum * ELF64_PROGRAM_HEADER_SIZE)
        .and_then(|end| bytes.get(phoff..end))
        .ok_or("program header table outside the file")?;
    let verdict = admit_native_abi_stamp(table, |offset, buf| {
        let offset = offset as usize;
        if offset >= bytes.len() {
            return Ok::<usize, std::convert::Infallible>(0);
        }
        let n = buf.len().min(bytes.len() - offset);
        buf[..n].copy_from_slice(&bytes[offset..offset + n]);
        Ok(n)
    })
    .unwrap_or_else(|never| match never {});
    match verdict {
        Ok(Some(revision)) if revision == NATIVE_ABI_REVISION_CURRENT => Ok(revision),
        Ok(Some(revision)) => Err(format!(
            "stamped revision {revision}, this build is {NATIVE_ABI_REVISION_CURRENT}; rebuild"
        )),
        Ok(None) => Err("no .note.rustos.abi stamp (PT_NOTE) found".into()),
        Err(NativeAbiRefusal::UnsupportedRevision(revision)) => {
            Err(format!("unsupported native ABI revision {revision}"))
        }
        Err(NativeAbiRefusal::MalformedNote) => Err("malformed or duplicate RustOS ABI note".into()),
    }
}

fn verify_native_elf_file(path: &Path) -> Result<()> {
    let bytes = fs::read(path)?;
    verify_native_elf_bytes(&bytes)
        .map(|_| ())
        .map_err(|why| anyhow::anyhow!("{}: {why}", path.display()))
}

fn source_expands_stamp(package_root: &Path) -> Result<bool> {
    for entry in WalkDir::new(package_root.join("src")) {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(entry.path())?;
            if text.contains("native_abi_note!(") || text.contains("svc_runtime::entry!(") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

pub(crate) fn check_native_abi_stamps(config: &Config) -> Result<()> {
    let manifests = load_manifests(&config.root_dir)?;
    let stamp_changed = STAMP_SOURCES
        .iter()
        .filter_map(|rel| mtime(&config.root_dir.join(rel)))
        .max();
    let mut errors = Vec::new();
    let mut count = 0;
    for manifest in manifests.iter().filter(|m| is_native_package(m)) {
        count += 1;
        match source_expands_stamp(&manifest.package_root) {
            Ok(true) => {}
            Ok(false) => errors.push(format!(
                "{}: package depends on an ABI crate but never expands rustos_user_abi::native_abi_note! (or entry!)",
                manifest.id
            )),
            Err(error) => errors.push(format!("{}: {error:#}", manifest.id)),
        }
        // An artifact built before the stamp sources last changed is stale,
        // not wrong; `stage` verifies it once it has been rebuilt.
        let artifact = manifest.artifact_path(config);
        if let (Some(built), Some(changed)) = (mtime(&artifact), stamp_changed)
            && built >= changed
            && let Err(error) = verify_native_elf_file(&artifact)
        {
            errors.push(format!("{}: {error:#}", manifest.id));
        }
    }
    if count == 0 {
        errors.push("no native packages found; the dependency rule is broken".into());
    }
    if !errors.is_empty() {
        bail!("native ABI stamp check failed:\n  {}", errors.join("\n  "));
    }
    Ok(())
}

/// Stage-time gate over every native ELF in the staged image, including the
/// bootstrap-allowlist services `loaderd` never sees.
pub(crate) fn verify_staged_native_elfs(config: &Config, manifests: &[PackageManifest]) -> Result<()> {
    for manifest in manifests.iter().filter(|m| is_native_package(m)) {
        let image = manifest.image_path(config);
        verify_native_elf_file(&image)
            .with_context(|| format!("native ABI stamp check of staged package {}", manifest.id))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::abi_revision::encode_native_abi_note;

    /// Minimal ELF64: header, one PT_NOTE, then the note bytes.
    fn synthetic(note: &[u8]) -> Vec<u8> {
        let mut f = vec![0_u8; 64 + 56];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2;
        f[32..40].copy_from_slice(&64_u64.to_le_bytes());
        f[54..56].copy_from_slice(&56_u16.to_le_bytes());
        f[56..58].copy_from_slice(&1_u16.to_le_bytes());
        f[64..68].copy_from_slice(&4_u32.to_le_bytes());
        f[72..80].copy_from_slice(&120_u64.to_le_bytes());
        f[96..104].copy_from_slice(&(note.len() as u64).to_le_bytes());
        f[112..120].copy_from_slice(&4_u64.to_le_bytes());
        f.extend_from_slice(note);
        f
    }

    #[test]
    fn synthetic_stamps_are_judged() {
        let ok = synthetic(&encode_native_abi_note(NATIVE_ABI_REVISION_CURRENT));
        assert_eq!(verify_native_elf_bytes(&ok), Ok(NATIVE_ABI_REVISION_CURRENT));
        let newer = synthetic(&encode_native_abi_note(NATIVE_ABI_REVISION_CURRENT + 1));
        assert!(verify_native_elf_bytes(&newer).unwrap_err().contains("unsupported"));
        assert!(verify_native_elf_bytes(&synthetic(&[])).unwrap_err().contains("no .note"));
        assert!(verify_native_elf_bytes(b"MZ").is_err());
    }

    #[test]
    fn native_set_matches_reality() {
        let config = Config::from_env().unwrap();
        let manifests = load_manifests(&config.root_dir).unwrap();
        let mut native: Vec<_> = manifests
            .iter()
            .filter(|m| is_native_package(m))
            .map(|m| m.id.as_str())
            .collect();
        native.sort();
        assert_eq!(
            native,
            [
                "devmgrd", "initd", "inputd", "ipcbench", "loaderd", "netd", "pagerd", "procd",
                "rootd", "runtimed", "storaged", "syscalld", "uiserver", "vfsd", "wayclick"
            ]
        );
        for (id, _) in NOT_STAMPED {
            assert!(manifests.iter().any(|m| m.id == *id), "stale NOT_STAMPED {id}");
        }
    }

    #[test]
    fn workspace_native_packages_are_stamped() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config = Config::from_env().unwrap();
        assert_eq!(config.root_dir.canonicalize().unwrap(), root.canonicalize().unwrap());
        check_native_abi_stamps(&config).unwrap();
    }

    #[test]
    fn built_native_elf_carries_the_current_stamp_when_present() {
        let config = Config::from_env().unwrap();
        let manifests = load_manifests(&config.root_dir).unwrap();
        let fresh_after = STAMP_SOURCES
            .iter()
            .filter_map(|rel| mtime(&config.root_dir.join(rel)))
            .max();
        for manifest in manifests.iter().filter(|m| is_native_package(m)) {
            let artifact = manifest.artifact_path(&config);
            if let (Some(built), Some(changed)) = (mtime(&artifact), fresh_after)
                && built >= changed
            {
                verify_native_elf_file(&artifact).unwrap();
            }
        }
    }
}
