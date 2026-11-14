//! Native ABI revision: one monotonic integer stamped into every native
//! (static-PIE, `no_std`) RustOS binary at build time.
//!
//! * **Owner:** this module is the only definition of the current revision,
//!   the supported set and the note layout. `rustos-svc-runtime` emits the
//!   note from [`NATIVE_ABI_REVISION_CURRENT`], `rustos-image-admission`
//!   selects and bounds the ELF note segments, `loaderd` refuses unsupported
//!   stamps and `xtask` verifies staged binaries against the same constants.
//! * **Not authority:** the stamp is a self-declared compatibility hint. It
//!   never selects a personality, never feeds `personality_gate` or `rootd`
//!   authorization and grants nothing. A binary may omit, forge or lie about
//!   it; the only consequence of a missing stamp is "treated as an ordinary
//!   ELF", and a forged one can only make `loaderd` refuse it.
//! * **Not Fuchsia's scheme:** one integer, bumped only for a semantic break
//!   (`docs/architecture/evolution-rules.md`, "Native ABI revision").

/// Revision implemented by this build of the system.
pub const NATIVE_ABI_REVISION_CURRENT: u32 = 1;

/// Revisions `loaderd` still admits. Contains [`NATIVE_ABI_REVISION_CURRENT`];
/// an older revision stays here only while a compatibility shim for it exists.
pub const NATIVE_ABI_SUPPORTED_REVISIONS: &[u32] = &[NATIVE_ABI_REVISION_CURRENT];

/// ELF section carrying the note.
pub const NATIVE_ABI_NOTE_SECTION: &str = ".note.rustos.abi";
/// Note owner name, without the terminating NUL.
pub const NATIVE_ABI_NOTE_NAME: &[u8; 6] = b"RustOS";
/// Note type for the native ABI revision.
pub const NATIVE_ABI_NOTE_TYPE: u32 = 1;
/// `descsz`: one little-endian `u32`.
pub const NATIVE_ABI_NOTE_DESC_SIZE: u32 = 4;
/// `namesz`: "RustOS" plus the NUL.
pub const NATIVE_ABI_NOTE_NAME_SIZE: u32 = 7;
/// Encoded size of the note emitted by the runtime (12-byte header, name
/// padded to 8, 4-byte descriptor).
pub const NATIVE_ABI_NOTE_ENCODED_SIZE: usize = 24;

/// Stamp the image with the native ABI revision it was built against.
///
/// Emits one 24-byte ELF note (`RustOS`, type 1, little-endian revision from
/// `rustos_user_abi::abi_revision::NATIVE_ABI_REVISION_CURRENT`) in the
/// 4-aligned `.note.rustos.abi` section; lld turns it into a `PT_NOTE`.
/// Every binary crate that depends directly on `rustos-user-abi` expands it
/// once in its executable's root (`rustos_user_abi::native_abi_note!();`).
/// `rustos_svc_runtime::entry!` expands it for static-PIE services; a service
/// with its own `_start` (rootd) invokes it explicitly. The exported hidden symbol makes a second
/// expansion in one image a link error, because two notes are `Malformed`.
///
/// The stamp is a self-declared compatibility hint read by `loaderd`; it is
/// not an authority claim and nothing may be granted because it is present.
/// `cargo xtask check` verifies that every packaged executable depending on `rustos-user-abi` expands it and
/// that every staged native ELF carries the current revision.
#[macro_export]
macro_rules! native_abi_note {
    () => {
        ::core::arch::global_asm!(
            ".pushsection .note.rustos.abi,\"a\",@note",
            ".balign 4",
            ".hidden __rustos_native_abi_note",
            ".global __rustos_native_abi_note",
            ".type __rustos_native_abi_note, @object",
            "__rustos_native_abi_note:",
            "    .long 7", // namesz: "RustOS\0"
            "    .long 4", // descsz: one u32
            "    .long 1", // NATIVE_ABI_NOTE_TYPE
            "    .asciz \"RustOS\"",
            "    .balign 4",
            "    .long {revision}",
            ".size __rustos_native_abi_note, .-__rustos_native_abi_note",
            ".popsection",
            revision = const $crate::abi_revision::NATIVE_ABI_REVISION_CURRENT,
        );
    };
}

/// Result of scanning one `PT_NOTE` segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoteOutcome {
    /// No RustOS ABI note (or the notes could not be walked far enough to
    /// find one): an ordinary ELF.
    Absent,
    /// Exactly one well-formed note carrying this revision. Whether it is
    /// supported is [`is_supported_native_abi_revision`].
    Valid(u32),
    /// A RustOS ABI note that is structurally wrong, or more than one.
    Malformed,
}

pub fn is_supported_native_abi_revision(revision: u32) -> bool {
    NATIVE_ABI_SUPPORTED_REVISIONS.contains(&revision)
}

/// Encodes the note exactly as the runtime emits it.
pub const fn encode_native_abi_note(revision: u32) -> [u8; NATIVE_ABI_NOTE_ENCODED_SIZE] {
    let mut out = [0_u8; NATIVE_ABI_NOTE_ENCODED_SIZE];
    let n = NATIVE_ABI_NOTE_NAME_SIZE.to_le_bytes();
    let d = NATIVE_ABI_NOTE_DESC_SIZE.to_le_bytes();
    let t = NATIVE_ABI_NOTE_TYPE.to_le_bytes();
    let r = revision.to_le_bytes();
    let mut i = 0;
    while i < 4 {
        out[i] = n[i];
        out[4 + i] = d[i];
        out[8 + i] = t[i];
        out[20 + i] = r[i];
        i += 1;
    }
    let mut i = 0;
    while i < 6 {
        out[12 + i] = NATIVE_ABI_NOTE_NAME[i];
        i += 1;
    }
    out
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    let s = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

/// Scans the bytes of one `PT_NOTE` segment. `align` is the segment's
/// `p_align` rounding for name and descriptor (4 or 8; anything else is
/// treated as 4). The runtime's own note is laid out for 4. Notes of other owners are skipped. If the framing of a
/// foreign note is broken the walk stops and reports `Absent`: that segment is
/// not ours to judge, and an absent stamp only means "ordinary ELF". A note
/// owned by RustOS with type [`NATIVE_ABI_NOTE_TYPE`] must be exactly
/// well-formed, and at most one may exist.
pub fn parse_native_abi_note_aligned(note_bytes: &[u8], align: usize) -> NoteOutcome {
    let align = if align == 8 { 8 } else { 4 };
    let mut at = 0_usize;
    let mut found: Option<u32> = None;
    while at < note_bytes.len() {
        let (Some(namesz), Some(descsz), Some(ntype)) = (
            le32(note_bytes, at),
            le32(note_bytes, at + 4),
            le32(note_bytes, at + 8),
        ) else {
            break;
        };
        let name_at = at + 12;
        // A name cut short by the end of the segment still counts as ours
        // while the bytes present are a prefix of the owner name, so a
        // truncated stamp is Malformed rather than silently Absent.
        let avail = &note_bytes[name_at.min(note_bytes.len())
            ..name_at.saturating_add(7).min(note_bytes.len())];
        let ours = namesz == NATIVE_ABI_NOTE_NAME_SIZE && b"RustOS\0".starts_with(avail);
        if ours && ntype == NATIVE_ABI_NOTE_TYPE {
            if descsz != NATIVE_ABI_NOTE_DESC_SIZE {
                return NoteOutcome::Malformed;
            }
            let Some(desc_at) = align_up(name_at + namesz as usize, align) else {
                return NoteOutcome::Malformed;
            };
            let Some(value) = le32(note_bytes, desc_at) else {
                return NoteOutcome::Malformed;
            };
            if found.is_some() {
                return NoteOutcome::Malformed;
            }
            found = Some(value);
        }
        let Some(desc_at) = name_at
            .checked_add(namesz as usize)
            .and_then(|e| align_up(e, align))
        else {
            break;
        };
        let Some(next) = desc_at
            .checked_add(descsz as usize)
            .and_then(|e| align_up(e, align))
        else {
            break;
        };
        at = next;
    }
    match found {
        Some(revision) => NoteOutcome::Valid(revision),
        None => NoteOutcome::Absent,
    }
}

/// [`parse_native_abi_note_aligned`] with the default 4-byte note alignment.
pub fn parse_native_abi_note(note_bytes: &[u8]) -> NoteOutcome {
    parse_native_abi_note_aligned(note_bytes, 4)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn foreign(name: &[u8], ntype: u32, desc: &[u8], align: usize) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&(name.len() as u32).to_le_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        v.extend_from_slice(&ntype.to_le_bytes());
        v.extend_from_slice(name);
        while v.len() % align != 0 {
            v.push(0);
        }
        v.extend_from_slice(desc);
        while v.len() % align != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn current_is_supported_and_listed() {
        assert!(NATIVE_ABI_SUPPORTED_REVISIONS.contains(&NATIVE_ABI_REVISION_CURRENT));
        assert!(is_supported_native_abi_revision(NATIVE_ABI_REVISION_CURRENT));
        assert!(!is_supported_native_abi_revision(0));
    }

    #[test]
    fn good_note_round_trips() {
        let note = encode_native_abi_note(1);
        assert_eq!(parse_native_abi_note(&note), NoteOutcome::Valid(1));
        // The emitted note uses 4-byte padding, so its section (and the
        // PT_NOTE lld builds from it) is 4-aligned; see the runtime macro.
    }

    #[test]
    fn unsupported_value_parses_but_is_not_supported() {
        let note = encode_native_abi_note(0xffff_fff0);
        assert_eq!(parse_native_abi_note(&note), NoteOutcome::Valid(0xffff_fff0));
        assert!(!is_supported_native_abi_revision(0xffff_fff0));
    }

    #[test]
    fn bad_descriptor_length_is_malformed() {
        let n = foreign(b"RustOS\0", 1, &[1, 0, 0, 0, 0, 0, 0, 0], 4);
        assert_eq!(parse_native_abi_note(&n), NoteOutcome::Malformed);
        let n = foreign(b"RustOS\0", 1, &[1, 0], 4);
        assert_eq!(parse_native_abi_note(&n), NoteOutcome::Malformed);
    }

    #[test]
    fn wrong_name_or_type_is_absent() {
        let n = foreign(b"Rustos\0", 1, &1_u32.to_le_bytes(), 4);
        assert_eq!(parse_native_abi_note(&n), NoteOutcome::Absent);
        let n = foreign(b"RustOS", 1, &1_u32.to_le_bytes(), 4);
        assert_eq!(parse_native_abi_note(&n), NoteOutcome::Absent);
        let n = foreign(b"RustOS\0", 2, &1_u32.to_le_bytes(), 4);
        assert_eq!(parse_native_abi_note(&n), NoteOutcome::Absent);
    }

    #[test]
    fn truncated_note_is_malformed() {
        let note = encode_native_abi_note(1);
        for cut in 12..24 {
            assert_eq!(
                parse_native_abi_note(&note[..cut]),
                NoteOutcome::Malformed,
                "cut={cut}"
            );
        }
        // A header cut short before the name is not even recognisable.
        assert_eq!(parse_native_abi_note(&note[..11]), NoteOutcome::Absent);
        assert_eq!(parse_native_abi_note(&[]), NoteOutcome::Absent);
    }

    #[test]
    fn foreign_notes_and_alignment_padding_are_walked() {
        for align in [4_usize, 8] {
            let mut seg = foreign(b"GNU\0", 3, &[0xab; 20], align);
            seg.extend(foreign(b"RustOS\0", 1, &7_u32.to_le_bytes(), align));
            seg.extend(foreign(b"GNU\0", 5, &[0; 16], align));
            assert_eq!(
                parse_native_abi_note_aligned(&seg, align),
                NoteOutcome::Valid(7),
                "align={align}"
            );
        }
    }

    #[test]
    fn duplicate_notes_are_malformed() {
        let mut seg = encode_native_abi_note(1).to_vec();
        seg.extend_from_slice(&encode_native_abi_note(1));
        assert_eq!(parse_native_abi_note(&seg), NoteOutcome::Malformed);
    }

    #[test]
    fn broken_foreign_framing_is_absent_not_a_panic() {
        let mut seg = foreign(b"GNU\0", 3, &[0; 8], 4);
        seg[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_native_abi_note(&seg), NoteOutcome::Absent);
        let mut seg = std::vec![0xff_u8; 40];
        seg[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_native_abi_note(&seg), NoteOutcome::Absent);
    }

    #[test]
    fn encoder_matches_generic_layout() {
        let generic = foreign(b"RustOS\0", 1, &1_u32.to_le_bytes(), 4);
        assert_eq!(&encode_native_abi_note(1)[..], &generic[..]);
    }
}
