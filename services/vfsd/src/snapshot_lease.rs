//! Request-owned immutable snapshot descriptor; cache eviction owns a different fd.
//!
//! - Owner: vfsd snapshot workers retain each request's fd through its reply.
//! - Boundary: cache fd numbers are not durable handle-transfer authority.
//! - Lifecycle: duplicate under storage ownership, reply, then close on every exit.
//! - Concurrency: remount/eviction may retire the cache fd, never this lease.
//! - Failure: refused replies and failed duplication cannot leak request fds.
//! - Evidence: native eviction/reuse, reply-refusal, and byte-limit regressions.

const EOVERFLOW: i32 = 75;
/// One descriptor held through the terminal handle-bearing reply. The cache
/// owner must duplicate under its storage guard before returning this lease.
pub struct ExecutableSnapshotLease<C: FnMut(i32)> {
    fd: i32,
    file_bytes: u64,
    mount_generation: u64,
    close: C,
}

impl<C: FnMut(i32)> ExecutableSnapshotLease<C> {
    pub fn from_owned(fd: i32, file_bytes: u64, mount_generation: u64, close: C) -> Self {
        Self {
            fd,
            file_bytes,
            mount_generation,
            close,
        }
    }

    pub fn duplicate_cached(
        fd: i32,
        file_bytes: u64,
        mount_generation: u64,
        max_bytes: u64,
        duplicate: impl FnOnce(i32) -> Result<i32, i32>,
        close: C,
    ) -> Result<Self, i32> {
        if file_bytes > max_bytes {
            return Err(EOVERFLOW); // The current limit also applies on a cache hit.
        }
        let owned = duplicate(fd)?;
        Ok(Self::from_owned(owned, file_bytes, mount_generation, close))
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }
    pub fn file_bytes(&self) -> u64 {
        self.file_bytes
    }
    pub fn mount_generation(&self) -> u64 {
        self.mount_generation
    }
}

impl<C: FnMut(i32)> Drop for ExecutableSnapshotLease<C> {
    fn drop(&mut self) {
        (self.close)(self.fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, rc::Rc, vec::Vec};
    use core::cell::RefCell;

    #[derive(Default)]
    struct Descriptors {
        images: BTreeMap<i32, u64>,
        closed: Vec<i32>,
    }

    #[test]
    fn cached_lease_survives_eviction_and_descriptor_reuse() {
        let state = Rc::new(RefCell::new(Descriptors::default()));
        state.borrow_mut().images.insert(7, 100);
        let release = state.clone();
        let lease = ExecutableSnapshotLease::duplicate_cached(
            7,
            4096,
            9,
            4096,
            |fd| {
                let image = state.borrow().images[&fd];
                state.borrow_mut().images.insert(21, image);
                Ok(21)
            },
            move |fd| {
                let mut state = release.borrow_mut();
                state.images.remove(&fd);
                state.closed.push(fd);
            },
        )
        .unwrap();
        // Cache eviction closes A's cache descriptor; another image reuses it.
        state.borrow_mut().images.remove(&7);
        state.borrow_mut().images.insert(7, 200);
        assert_eq!(state.borrow().images[&lease.fd()], 100);
        assert_eq!(lease.file_bytes(), 4096);
        assert_eq!(lease.mount_generation(), 9);
        drop(lease); // Either successful or refused terminal reply releases it.
        assert_eq!(state.borrow().closed, [21]);
        assert_eq!(state.borrow().images[&7], 200);
    }

    #[test]
    fn failed_reply_or_duplicate_never_leaks_a_request_descriptor() {
        let closed = RefCell::new(Vec::new());
        let result: Result<(), i32> = (|| {
            let _lease =
                ExecutableSnapshotLease::from_owned(21, 4096, 9, |fd| closed.borrow_mut().push(fd));
            Err(11) // A refused handle-bearing reply still drops its lease.
        })();
        assert_eq!(result, Err(11));
        assert_eq!(*closed.borrow(), [21]);
        let result = ExecutableSnapshotLease::duplicate_cached(
            7,
            4096,
            9,
            4096,
            |_| Err(24),
            |fd| closed.borrow_mut().push(fd),
        );
        assert!(matches!(result, Err(24)));
        assert_eq!(*closed.borrow(), [21]);
    }

    #[test]
    fn cache_hit_obeys_the_current_request_byte_limit_before_duplication() {
        let result = ExecutableSnapshotLease::duplicate_cached(
            7,
            4096,
            9,
            4095,
            |_| panic!("oversized cache hit duplicated a descriptor"),
            |_| panic!("no lease was admitted"),
        );
        assert!(matches!(result, Err(EOVERFLOW)));
    }
}
