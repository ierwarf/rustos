use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, bail};
use ed25519_dalek::{Signer, SigningKey};

use crate::Result;

const STORAGE_EPOCH_PRIVATE_KEY_BYTES: usize = 32;

pub(crate) fn load_or_create_signing_key(path: &Path) -> Result<SigningKey> {
    let missing = match fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect storage epoch key {}", path.display()));
        }
    };
    if missing {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("create storage epoch key directory {}", parent.display())
            })?;
        }
        let mut seed = [0_u8; STORAGE_EPOCH_PRIVATE_KEY_BYTES];
        File::open("/dev/urandom")
            .context("open kernel random source for storage epoch key")?
            .read_exact(&mut seed)
            .context("read storage epoch signing seed")?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(&seed)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create storage epoch signing key {}", path.display())
                });
            }
        }
        seed.fill(0);
    }
    load_signing_key(path)
}

pub(crate) fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .with_context(|| format!("open storage epoch signing key {}", path.display()))?;
    let metadata = file
        .metadata()
        .context("inspect opened storage epoch signing key")?;
    if !metadata.file_type().is_file() {
        bail!("storage epoch signing key must be a regular non-symlink file");
    }
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o077 != 0 {
        bail!("storage epoch signing key must be owned by the caller and mode 0600 or stricter");
    }
    if metadata.len() != STORAGE_EPOCH_PRIVATE_KEY_BYTES as u64 {
        bail!("storage epoch signing key must contain exactly 32 raw bytes");
    }
    let mut seed = [0_u8; STORAGE_EPOCH_PRIVATE_KEY_BYTES];
    file.read_exact(&mut seed)?;
    if seed.iter().all(|byte| *byte == 0) {
        bail!("storage epoch signing key must not be the all-zero seed");
    }
    let key = SigningKey::from_bytes(&seed);
    seed.fill(0);
    Ok(key)
}

pub(crate) fn sign_epoch(
    key: &SigningKey,
    header: driver_domain_protocol::DvmBlockHeader,
) -> driver_domain_protocol::DvmBlockHeader {
    header.with_epoch_signature(key.sign(&header.epoch_signing_bytes()).to_bytes())
}
