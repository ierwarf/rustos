//! Resident-page lease on a Job charge group for kernel-local objects.
//!
//! - **Owner:** `kernel-ps`; pages are billed to the creating or sending Job's
//!   charge group (`kernel_mm::api::charge`), `None` bills nothing.
//! - **Lifecycle:** `acquire` reserves before the caller allocates; the lease
//!   refunds exactly once on drop and pins the group slot against reuse while
//!   pages are outstanding (`charge::retain`/`release`).
//! - **Failure:** refusal returns `None` and changes nothing.
//! - **Evidence:** `memory-charge-conservation`.
use kernel_mm::api::charge;

/// Resident pages billed to a Job charge group, refunded exactly once when
/// the lease drops. The lease pins the group slot against reuse while pages
/// are outstanding (`charge::retain`). `group: None` bills nothing.
#[derive(Debug)]
pub(crate) struct PageLease {
    group: Option<usize>,
    pages: u64,
}

impl PageLease {
    pub(crate) fn acquire(group: Option<usize>, pages: u64) -> Option<Self> {
        if let Some(group) = group {
            charge::retain(group);
            if !charge::try_charge(group, pages) {
                charge::release(group);
                return None;
            }
        }
        Some(Self { group, pages })
    }

    /// Grows or shrinks the lease to `pages`; a refused growth changes nothing.
    pub(crate) fn resize(&mut self, pages: u64) -> bool {
        if let Some(group) = self.group {
            if pages > self.pages {
                if !charge::try_charge(group, pages - self.pages) {
                    return false;
                }
            } else if pages < self.pages {
                charge::uncharge(group, self.pages - pages);
            }
        }
        self.pages = pages;
        true
    }
}

impl Drop for PageLease {
    fn drop(&mut self) {
        if let Some(group) = self.group {
            charge::uncharge(group, self.pages);
            charge::release(group);
        }
    }
}
