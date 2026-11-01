//! Release bundle container and verification, host-testable.
//!
//! A bundle is one file on a sealed read-only volume:
//! `magic[8] | candidate_len u32 | previous_len u32 | envelope[192] |
//! candidate | previous` (little endian, no padding, no trailing bytes). The
//! parser never allocates and every length is bounded before any slice is cut.
use rustos_release_verifier::{self as verifier, KeyRing, Verified, ENVELOPE_BYTES};

pub const BUNDLE_MAGIC: [u8; 8] = *b"RSRELv1\0";
pub const BUNDLE_HEADER_BYTES: usize = 16;
/// Per-image bound for staging: the verifier's format ceiling is larger, but
/// loaderd must hold both images in memory to hash them.
pub const MAX_STAGED_IMAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundleError {
    /// Not a bundle: wrong magic, zero or oversized image, or wrong geometry.
    Invalid,
    Image(verifier::Error),
}

pub struct Bundle<'a> {
    pub envelope: &'a [u8],
    pub candidate: &'a [u8],
    pub previous: &'a [u8],
}

fn le32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
}

/// Total file length a bundle header declares, or why it is refused.
pub fn bundle_length(header: &[u8]) -> Result<usize, BundleError> {
    if header.len() < BUNDLE_HEADER_BYTES || header[..8] != BUNDLE_MAGIC {
        return Err(BundleError::Invalid);
    }
    let candidate = le32(header, 8);
    let previous = le32(header, 12);
    if candidate == 0
        || previous == 0
        || candidate > MAX_STAGED_IMAGE_BYTES
        || previous > MAX_STAGED_IMAGE_BYTES
    {
        return Err(BundleError::Invalid);
    }
    Ok(BUNDLE_HEADER_BYTES + ENVELOPE_BYTES + candidate + previous)
}

pub fn parse_bundle(bytes: &[u8]) -> Result<Bundle<'_>, BundleError> {
    if bundle_length(bytes)? != bytes.len() {
        return Err(BundleError::Invalid);
    }
    let candidate = le32(bytes, 8);
    let envelope_end = BUNDLE_HEADER_BYTES + ENVELOPE_BYTES;
    Ok(Bundle {
        envelope: &bytes[BUNDLE_HEADER_BYTES..envelope_end],
        candidate: &bytes[envelope_end..envelope_end + candidate],
        previous: &bytes[envelope_end + candidate..],
    })
}

/// Verifies a whole bundle against `ring`; touches nothing else.
pub fn admit_bundle(
    ring: &KeyRing<'_>,
    bytes: &[u8],
    service: u64,
    durable_floor: u64,
) -> Result<Verified, BundleError> {
    let bundle = parse_bundle(bytes)?;
    verifier::verify_release(
        ring,
        bundle.envelope,
        bundle.candidate,
        bundle.previous,
        service,
        durable_floor,
    )
    .map_err(BundleError::Image)
}

/// Decodes exactly 64 lowercase hex digits into a public key.
pub fn parse_hex_key(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut key = [0u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        key[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;
    use alloc::string::ToString;
    use rustos_release_verifier::{Manifest, TrustedKey, MANIFEST_BYTES};
    use sha2::{Digest, Sha256};

    fn sha(bytes: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&Sha256::digest(bytes));
        out
    }

    /// Builds a signed bundle exactly as the xtask does, with a test seed.
    fn bundle(candidate: &[u8], previous: &[u8], service: u64, generation: u64) -> (Vec<u8>, TrustedKey) {
        use ed25519_dalek::{Signer, SigningKey};
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let manifest = Manifest {
            service,
            generation,
            minimum_floor: generation - 1,
            image_len: candidate.len() as u64,
            image_sha256: sha(candidate),
            signer_id: [5u8; 16],
            previous_sha256: sha(previous),
        };
        let bytes = verifier::encode(manifest).unwrap();
        let mut message = Vec::new();
        message.extend_from_slice(verifier::DOMAIN);
        message.extend_from_slice(&bytes);
        let signature = signing.sign(&message).to_bytes();
        let mut file = Vec::new();
        file.extend_from_slice(&BUNDLE_MAGIC);
        file.extend_from_slice(&(candidate.len() as u32).to_le_bytes());
        file.extend_from_slice(&(previous.len() as u32).to_le_bytes());
        file.extend_from_slice(&bytes);
        file.extend_from_slice(&signature);
        file.extend_from_slice(candidate);
        file.extend_from_slice(previous);
        assert_eq!(MANIFEST_BYTES + 64, ENVELOPE_BYTES);
        (
            file,
            TrustedKey {
                signer_id: [5u8; 16],
                service,
                verifying_key: signing.verifying_key().to_bytes(),
                minimum_generation: 1,
                revoked: false,
            },
        )
    }

    #[test]
    fn a_signed_bundle_is_admitted_and_every_tamper_is_refused() {
        let (file, key) = bundle(b"new image bytes", b"old image bytes", 3, 2);
        let keys = [key];
        let ring = KeyRing::new(&keys).unwrap();
        assert_eq!(admit_bundle(&ring, &file, 3, 1).unwrap().manifest().generation, 2);
        assert_eq!(
            admit_bundle(&ring, &file, 4, 1).err(),
            Some(BundleError::Image(verifier::Error::WrongService))
        );
        assert_eq!(
            admit_bundle(&ring, &file, 3, 2).err(),
            Some(BundleError::Image(verifier::Error::RollbackFloor))
        );
        let mut flipped = file.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert_eq!(
            admit_bundle(&ring, &flipped, 3, 1).err(),
            Some(BundleError::Image(verifier::Error::PreviousImage))
        );
        let mut forged = file.clone();
        forged[BUNDLE_HEADER_BYTES + MANIFEST_BYTES] ^= 1;
        assert_eq!(
            admit_bundle(&ring, &forged, 3, 1).err(),
            Some(BundleError::Image(verifier::Error::InvalidSignature))
        );
        let empty = KeyRing::new(&[]).unwrap();
        assert_eq!(
            admit_bundle(&empty, &file, 3, 1).err(),
            Some(BundleError::Image(verifier::Error::UntrustedKey)),
            "the shipped empty ring refuses every release"
        );
    }

    #[test]
    fn bundle_geometry_is_exact() {
        let (file, _) = bundle(b"candidate", b"previous", 3, 2);
        assert!(parse_bundle(&file).is_ok());
        let mut trailing = file.clone();
        trailing.push(0);
        assert_eq!(parse_bundle(&trailing).err(), Some(BundleError::Invalid));
        assert_eq!(parse_bundle(&file[..file.len() - 1]).err(), Some(BundleError::Invalid));
        let mut magic = file.clone();
        magic[0] ^= 1;
        assert_eq!(bundle_length(&magic), Err(BundleError::Invalid));
        let mut huge = file.clone();
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(bundle_length(&huge), Err(BundleError::Invalid));
        let mut zero = file;
        zero[12..16].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(bundle_length(&zero), Err(BundleError::Invalid));
    }

    #[test]
    fn hex_keys_are_strict() {
        assert_eq!(parse_hex_key(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(parse_hex_key(&"AB".repeat(32)), None, "uppercase is not canonical");
        assert_eq!(parse_hex_key(&"a".repeat(63)), None);
        assert_eq!(parse_hex_key(&format!("{}zz", "a".repeat(62))), None);
    }
}
