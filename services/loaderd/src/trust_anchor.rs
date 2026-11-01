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

pub(super) fn parse(bytes: &[u8]) -> Result<Vec<TrustedKey>, Error> {
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

/// The keys sealed into this loaderd image.
pub(super) fn sealed_keys() -> Result<Vec<TrustedKey>, Error> {
    parse(SEALED)
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
