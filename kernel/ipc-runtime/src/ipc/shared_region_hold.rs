//! Retained generational ownership of one shared-region mapping.

use super::{KernelSharedRegionHandle, release_shared_region, retain_shared_region};

#[derive(Debug)]
pub struct KernelSharedRegionMappingHold {
    region: KernelSharedRegionHandle,
}

impl KernelSharedRegionMappingHold {
    pub(super) const fn new(region: KernelSharedRegionHandle) -> Self {
        Self { region }
    }

    /// Stable generational backing identity retained for this mapping.
    pub const fn identity(&self) -> u64 {
        self.region.raw()
    }
}

impl Clone for KernelSharedRegionMappingHold {
    fn clone(&self) -> Self {
        assert!(
            retain_shared_region(self.region),
            "cloned a stale shared-region mapping hold"
        );
        Self::new(self.region)
    }
}

impl Drop for KernelSharedRegionMappingHold {
    fn drop(&mut self) {
        release_shared_region(self.region);
    }
}
