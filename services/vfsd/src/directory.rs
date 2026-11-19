//! Directory cookies and checkpoint replay for the VFS receive owner.
//!
//! Zero rewinds. Namespace entries occupy the low range; backing entries use
//! a disjoint high range so a mount appearing/disappearing never rebases them.
//! Cookies are opaque, signed-positive `d_off` values, not entry counts.

use sha2::{Digest, Sha256};

pub const FILE_COOKIE_BASE: u64 = 1 << 62;

pub fn backing_cookie(position: u64) -> Option<u64> {
    (position > 0 && position < FILE_COOKIE_BASE).then(|| FILE_COOKIE_BASE + position)
}

/// Emit namespace entries first; they shadow backing names for every backend.
/// Both streams are ordered by their own stable positions and `after` is
/// exclusive. `visit` returns false when the next record does not fit.
pub fn visit_merged<T, E>(
    after: u64,
    namespace: &[(u64, T)],
    backing: impl IntoIterator<Item = (u64, T)>,
    shadows: impl Fn(&T, &T) -> bool,
    mut visit: impl FnMut(u64, &T) -> Result<bool, E>,
) -> Result<(), E> {
    for (cookie, entry) in namespace {
        if *cookie > after && !visit(*cookie, entry)? {
            return Ok(());
        }
    }
    for (position, entry) in backing {
        let Some(cookie) = backing_cookie(position) else { continue };
        if cookie > after && !namespace.iter().any(|(_, overlay)| shadows(overlay, &entry))
            && !visit(cookie, &entry)?
        {
            break;
        }
    }
    Ok(())
}

/// Checkpoint space is fixed. Keep the admitted buffer size (16 bits) and a
/// truncated SHA-256 of the encoded reply (48 bits) in the existing result
/// field. The checkpoint's cursor separately records the resulting cookie.
/// A replay with changed bytes or position fails closed instead of returning
/// a different page that merely happens to contain the same number of entries.
fn reply_tag(requested: u16, bytes: &[u8]) -> u64 {
    let digest = Sha256::digest(bytes);
    let mut tag = [0; 8];
    tag[..2].copy_from_slice(&requested.to_le_bytes());
    tag[2..].copy_from_slice(&digest[..6]);
    u64::from_le_bytes(tag)
}

pub fn checkpoint_reply(requested: usize, bytes: &[u8]) -> Option<u64> {
    Some(reply_tag(u16::try_from(requested).ok()?, bytes))
}

pub fn checkpoint_request_size(tag: u64) -> usize {
    usize::from(tag as u16)
}

pub fn replay_matches(tag: u64, recorded_end: u64, requested: usize, end: u64, bytes: &[u8]) -> bool {
    recorded_end == end && checkpoint_reply(requested, bytes) == Some(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_disjoint_positive_and_leave_zero_for_rewind() {
        assert_eq!(backing_cookie(0), None);
        assert_eq!(backing_cookie(1), Some(FILE_COOKIE_BASE + 1));
        assert_eq!(backing_cookie(FILE_COOKIE_BASE - 1), Some(i64::MAX as u64));
        assert_eq!(backing_cookie(FILE_COOKIE_BASE), None);
    }

    #[test]
    fn overlay_shadows_backing_and_mutations_do_not_rebase_the_cursor() {
        let mut page = alloc::vec::Vec::new();
        let mut visit = |cookie, name: &&'static str| {
            page.push((cookie, *name));
            Ok::<_, ()>(true)
        };
        visit_merged(0, &[(4, "data")], [(32, "DATA"), (64, "a"), (96, "b")],
            |a, b| a.eq_ignore_ascii_case(b), &mut visit).unwrap();
        assert_eq!(page, [(4, "data"), (FILE_COOKIE_BASE + 64, "a"), (FILE_COOKIE_BASE + 96, "b")]);
        page.clear();
        visit_merged(FILE_COOKIE_BASE + 64, &[(5, "other")], [(96, "b")],
            |a, b| a == b, |cookie, name| { page.push((cookie, *name)); Ok::<_, ()>(true) }).unwrap();
        assert_eq!(page, [(FILE_COOKIE_BASE + 96, "b")]);
    }

    #[test]
    fn replay_rejects_a_different_page_of_the_same_size() {
        let tag = checkpoint_reply(512, b"old").unwrap();
        assert_eq!(checkpoint_request_size(tag), 512);
        assert!(replay_matches(tag, 20, 512, 20, b"old"));
        assert!(!replay_matches(tag, 20, 512, 20, b"new"));
        assert!(!replay_matches(tag, 20, 512, 21, b"old"));
        assert!(!replay_matches(tag, 20, 511, 20, b"old"));
    }
}
