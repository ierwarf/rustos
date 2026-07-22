#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;
use rustos_user_abi::syscall::{VFS_IPC_OP_FTRUNCATE, VFS_IPC_OP_WRITE, WAITSET_MAX_INTERESTS};

pub const ENOENT: i32 = 2;
pub const EROFS: i32 = 30;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WaitSetInterestKey {
    pub target_fd: u64,
    pub provider: u16,
    pub object_id: u64,
    pub provider_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitSetInterestRecord {
    pub key: WaitSetInterestKey,
    pub events: u32,
    pub data: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitSetRegistryError {
    Exists,
    NotFound,
    Capacity,
    Overflow,
}

#[derive(Clone, Default)]
pub struct WaitSetRegistry {
    epolls: BTreeMap<u64, WaitSetEpoll>,
}

#[derive(Clone)]
struct WaitSetEpoll {
    interests: BTreeMap<WaitSetInterestKey, WaitSetInterestRecord>,
    refs: u64,
    cursor: usize,
}

impl WaitSetRegistry {
    pub fn create(&mut self, token: u64) -> Result<(), WaitSetRegistryError> {
        if token == 0 || self.epolls.contains_key(&token) {
            return Err(WaitSetRegistryError::Exists);
        }
        self.epolls.insert(
            token,
            WaitSetEpoll {
                interests: BTreeMap::new(),
                refs: 1,
                cursor: 0,
            },
        );
        Ok(())
    }

    pub fn acquire(&mut self, token: u64) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        epoll.refs = epoll
            .refs
            .checked_add(1)
            .ok_or(WaitSetRegistryError::Overflow)?;
        Ok(())
    }

    pub fn release(&mut self, token: u64) -> Result<(), WaitSetRegistryError> {
        let refs = self
            .epolls
            .get(&token)
            .map(|epoll| epoll.refs)
            .ok_or(WaitSetRegistryError::NotFound)?;
        if refs > 1 {
            self.epolls.get_mut(&token).unwrap().refs = refs - 1;
        } else {
            self.epolls.remove(&token);
        }
        Ok(())
    }

    pub fn add(
        &mut self,
        token: u64,
        interest: WaitSetInterestRecord,
    ) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        if epoll.interests.contains_key(&interest.key) {
            return Err(WaitSetRegistryError::Exists);
        }
        if epoll.interests.len() >= WAITSET_MAX_INTERESTS {
            return Err(WaitSetRegistryError::Capacity);
        }
        epoll.interests.insert(interest.key, interest);
        Ok(())
    }

    pub fn modify(
        &mut self,
        token: u64,
        interest: WaitSetInterestRecord,
    ) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        if !epoll.interests.contains_key(&interest.key) {
            return Err(WaitSetRegistryError::NotFound);
        }
        epoll.interests.insert(interest.key, interest);
        Ok(())
    }

    pub fn delete(
        &mut self,
        token: u64,
        key: WaitSetInterestKey,
    ) -> Result<(), WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        epoll
            .interests
            .remove(&key)
            .map(|_| ())
            .ok_or(WaitSetRegistryError::NotFound)
    }

    pub fn purge(&mut self, provider: u16, object_id: u64) -> bool {
        let mut changed = false;
        for epoll in self.epolls.values_mut() {
            let before = epoll.interests.len();
            epoll
                .interests
                .retain(|key, _| key.provider != provider || key.object_id != object_id);
            changed |= epoll.interests.len() != before;
            if epoll.interests.is_empty() {
                epoll.cursor = 0;
            } else {
                epoll.cursor %= epoll.interests.len();
            }
        }
        changed
    }

    pub fn snapshot(
        &mut self,
        token: u64,
        max: usize,
    ) -> Result<Vec<WaitSetInterestRecord>, WaitSetRegistryError> {
        let epoll = self
            .epolls
            .get_mut(&token)
            .ok_or(WaitSetRegistryError::NotFound)?;
        let count = epoll.interests.len();
        let start = epoll.cursor.min(count.saturating_sub(1));
        let snapshot = epoll
            .interests
            .values()
            .skip(start)
            .chain(epoll.interests.values().take(start))
            .take(max)
            .copied()
            .collect::<Vec<_>>();
        if count != 0 {
            epoll.cursor = (start + 1) % count;
        }
        Ok(snapshot)
    }
}

/// Persistent mutation remains unavailable until a journal/recovery protocol
/// is implemented. Keeping this decision in the testable policy library makes
/// the service dispatch and the formal admission model share one source gate.
pub const fn persistent_mutation_status(op: u16) -> Option<i32> {
    match op {
        VFS_IPC_OP_WRITE | VFS_IPC_OP_FTRUNCATE => Some(EROFS),
        _ => None,
    }
}

pub fn mkdir_policy(path: &str, euid: u32) -> i32 {
    let run_user_path = format!("/run/user/{euid}");
    if path == "/run" || path == "/run/user" || path == run_user_path.as_str() {
        0
    } else {
        EROFS
    }
}

pub fn unlink_policy(path: &str) -> i32 {
    if path.starts_with("/run/") {
        ENOENT
    } else {
        EROFS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn persistent_mutation_admission_remains_read_only() {
        assert_eq!(persistent_mutation_status(VFS_IPC_OP_WRITE), Some(EROFS));
        assert_eq!(
            persistent_mutation_status(VFS_IPC_OP_FTRUNCATE),
            Some(EROFS)
        );
        assert_eq!(persistent_mutation_status(0xffff), None);
        assert_eq!(mkdir_policy("/var/lib/rustos", 0), EROFS);
        assert_eq!(unlink_policy("/var/lib/rustos/state"), EROFS);
        assert_eq!(mkdir_policy("/run/user/1000", 1000), 0);
        assert_eq!(unlink_policy("/run/user/1000/socket"), ENOENT);
    }

    fn interest(fd: u64, object_id: u64) -> WaitSetInterestRecord {
        WaitSetInterestRecord {
            key: WaitSetInterestKey {
                target_fd: fd,
                provider: 2,
                object_id,
                provider_epoch: 7,
            },
            events: 1,
            data: object_id,
        }
    }

    #[test]
    fn epoll_membership_binds_open_description_and_purges_last_close() {
        let mut registry = WaitSetRegistry::default();
        registry.create(41).unwrap();
        registry.add(41, interest(5, 101)).unwrap();
        registry.add(41, interest(5, 102)).unwrap();
        assert_eq!(
            registry.snapshot(41, WAITSET_MAX_INTERESTS).unwrap().len(),
            2
        );
        assert!(registry.purge(2, 101));
        assert_eq!(
            registry.snapshot(41, WAITSET_MAX_INTERESTS).unwrap(),
            vec![interest(5, 102)]
        );
    }

    #[test]
    fn epoll_snapshot_rotates_a_persistently_ready_prefix() {
        let mut registry = WaitSetRegistry::default();
        registry.create(41).unwrap();
        for object in [101, 102, 103] {
            registry.add(41, interest(object, object)).unwrap();
        }
        let first = registry.snapshot(41, 1).unwrap()[0].key.object_id;
        let second = registry.snapshot(41, 1).unwrap()[0].key.object_id;
        assert_ne!(first, second);
    }
}
