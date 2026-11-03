//! Boot-sealed release signer keyring.
//!
//! The ring is compiled into the signed loaderd image from
//! `services/loaderd/trust/release-keys.bin`, so a key can only be added or
//! revoked by shipping a new signed loaderd; no service, path or IPC payload
//! can provision one. The shipped file is empty: with no trusted signer every
//! release is refused with `UntrustedKey`. That is the fail-closed default,
//! not a feature gap to paper over.
//!
//! Record (65 bytes, little endian): signer_id[16], service u64, verifying
//! key[32], minimum_generation u64, revoked u8 (0 or 1).
use alloc::vec::Vec;
use rustos_release_verifier::{Error, TrustedKey};

const RECORD: usize = 16 + 8 + 32 + 8 + 1;
static SEALED: &[u8] = include_bytes!("../trust/release-keys.bin");

pub fn parse(bytes: &[u8]) -> Result<Vec<TrustedKey>, Error> {
    if bytes.len() % RECORD != 0 {
        return Err(Error::Length);
    }
    let mut keys = Vec::new();
    keys.try_reserve_exact(bytes.len() / RECORD).map_err(|_| Error::Length)?;
    for record in bytes.chunks_exact(RECORD) {
        let mut signer_id = [0u8; 16];
        signer_id.copy_from_slice(&record[..16]);
        let mut verifying_key = [0u8; 32];
        verifying_key.copy_from_slice(&record[24..56]);
        let revoked = match record[64] {
            0 => false,
            1 => true,
            _ => return Err(Error::InvalidKey),
        };
        keys.push(TrustedKey {
            signer_id,
            service: u64::from_le_bytes(record[16..24].try_into().map_err(|_| Error::Length)?),
            verifying_key,
            minimum_generation: u64::from_le_bytes(
                record[56..64].try_into().map_err(|_| Error::Length)?,
            ),
            revoked,
        });
    }
    Ok(keys)
}

/// Signer id and service of the opt-in test key (see `test_key`).
pub const TEST_SIGNER_ID: [u8; 16] = [0x7e; 16];
pub const TEST_SERVICE: u64 = 3;

/// A key compiled in only when the image is built with
/// `RUSTOS_LOADERD_TEST_RELEASE_KEY=<64 hex digits>`. It trusts exactly one
/// signer for one service, from generation 1. The matching private seed is
/// public test material in the xtask, so an image built with this variable
/// must never ship: the default build leaves the ring empty and every release
/// is refused with `UntrustedKey`.
fn test_key() -> Option<TrustedKey> {
    let verifying_key = crate::release_bundle::parse_hex_key(option_env!("RUSTOS_LOADERD_TEST_RELEASE_KEY")?)?;
    Some(TrustedKey {
        signer_id: TEST_SIGNER_ID,
        service: TEST_SERVICE,
        verifying_key,
        minimum_generation: 1,
        revoked: false,
    })
}

/// The keys sealed into this loaderd image.
pub fn sealed_keys() -> Result<Vec<TrustedKey>, Error> {
    let mut keys = parse(SEALED)?;
    if let Some(key) = test_key() {
        keys.push(key);
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_release_verifier::KeyRing;

    fn record(signer: u8, service: u64, revoked: u8) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[signer; 16]);
        out.extend_from_slice(&service.to_le_bytes());
        out.extend_from_slice(&[signer ^ 0x5a; 32]);
        out.extend_from_slice(&7u64.to_le_bytes());
        out.push(revoked);
        out
    }

    #[test]
    fn the_shipped_ring_is_empty_so_every_signer_is_untrusted() {
        let keys = sealed_keys().unwrap();
        assert!(keys.is_empty());
        assert!(KeyRing::new(&keys).is_ok());
    }

    #[test]
    fn the_test_key_is_absent_from_a_default_build() {
        assert!(test_key().is_none(), "the shipped build trusts no test signer");
    }

    #[test]
    fn records_parse_exactly_and_reject_truncation_and_bad_flags() {
        let mut bytes = record(1, 3, 0);
        bytes.extend(record(2, 4, 1));
        let keys = parse(&bytes).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!((keys[0].service, keys[0].minimum_generation, keys[0].revoked), (3, 7, false));
        assert!(keys[1].revoked);
        assert_eq!(parse(&bytes[..bytes.len() - 1]), Err(Error::Length));
        assert_eq!(parse(&record(1, 3, 2)), Err(Error::InvalidKey));
    }

    #[test]
    fn duplicate_signers_and_zero_services_are_refused_by_the_ring() {
        let mut bytes = record(1, 3, 0);
        bytes.extend(record(1, 4, 0));
        let keys = parse(&bytes).unwrap();
        assert_eq!(KeyRing::new(&keys).err(), Some(Error::DuplicateSigner));
        let zero = parse(&record(1, 0, 0)).unwrap();
        assert_eq!(KeyRing::new(&zero).err(), Some(Error::UntrustedKey));
    }
}
