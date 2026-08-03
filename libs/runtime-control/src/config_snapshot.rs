use std::fs::File;
use std::io::{Error, ErrorKind};
use std::os::unix::fs::FileExt;
use std::path::Path;

const MAX_CONFIG_SNAPSHOT_BYTES: usize = 1024 * 1024;
const CONFIG_SNAPSHOT_CHUNK_BYTES: usize = 4096;

pub(super) fn read_config_snapshot(path: impl AsRef<Path>) -> Result<String, Error> {
    read_bounded_config_snapshot(path, MAX_CONFIG_SNAPSHOT_BYTES)
}

/// Reads one immutable configuration snapshot without advancing a shared VFS
/// open-description cursor. Callers handling smaller private contracts should
/// pass their protocol bound so an oversized image fails before allocation or
/// parsing rather than inheriting the registry-wide 1 MiB ceiling.
pub fn read_bounded_config_snapshot(
    path: impl AsRef<Path>,
    max_bytes: usize,
) -> Result<String, Error> {
    if max_bytes == 0 || max_bytes > MAX_CONFIG_SNAPSHOT_BYTES {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "configuration snapshot bound is outside the admitted range",
        ));
    }
    let file = File::open(path)?;
    read_config_snapshot_file(&file, max_bytes)
}

pub(super) fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn read_config_snapshot_file(file: &File, max_bytes: usize) -> Result<String, Error> {
    // These files are immutable policy snapshots, not streams. Explicit-offset
    // I/O avoids publishing a durable VFS cursor transition for every chunk;
    // that checkpoint path is serialized with latency-sensitive VFS requests.
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; CONFIG_SNAPSHOT_CHUNK_BYTES];
    loop {
        if bytes.len() == max_bytes {
            let mut extra = [0_u8; 1];
            match file.read_at(&mut extra, max_bytes as u64) {
                Ok(0) => break,
                Ok(_) => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("configuration snapshot exceeds {max_bytes} bytes"),
                    ));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }

        let read_len = chunk.len().min(max_bytes - bytes.len());
        let offset = bytes.len() as u64;
        match file.read_at(&mut chunk[..read_len], offset) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(bytes).map_err(|error| Error::new(ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::{
        read_bounded_config_snapshot, read_config_snapshot_file, MAX_CONFIG_SNAPSHOT_BYTES,
    };
    use std::fs::File;
    use std::io::{Seek, SeekFrom};

    #[test]
    fn config_snapshot_read_does_not_move_the_open_file_cursor() {
        let mut file = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("open runtime-control manifest");
        file.seek(SeekFrom::Start(3)).expect("seed cursor");

        let contents =
            read_config_snapshot_file(&file, 1024 * 1024).expect("read configuration snapshot");

        assert!(contents.contains("name = \"runtime-control\""));
        assert_eq!(file.stream_position().expect("read cursor"), 3);
    }

    #[test]
    fn bounded_config_snapshot_rejects_the_first_byte_past_its_contract() {
        let file = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("open runtime-control manifest");
        let error = read_config_snapshot_file(&file, 8).expect_err("snapshot must exceed bound");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn config_snapshot_bound_must_be_nonzero_and_registry_bounded() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        for invalid in [0, MAX_CONFIG_SNAPSHOT_BYTES + 1] {
            let error = read_bounded_config_snapshot(path, invalid)
                .expect_err("invalid bound must fail before opening or reading");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }
}
