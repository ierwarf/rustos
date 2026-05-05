pub use crate::ipc::{IpcError, KernelSharedRegionHandle};

pub mod region {
    pub use crate::ipc::IpcError;
    pub use crate::ipc::KernelSharedRegionHandle;

    pub fn create(byte_len: usize) -> Result<KernelSharedRegionHandle, IpcError> {
        crate::ipc::create_shared_region(byte_len)
    }

    pub fn map(region: KernelSharedRegionHandle) -> Option<(*mut u8, usize)> {
        crate::ipc::map_shared_region(region)
    }

    pub fn frames(region: KernelSharedRegionHandle) -> Option<alloc::vec::Vec<u64>> {
        crate::ipc::shared_region_frames(region)
    }
}

pub use region::{
    create as create_shared_region, frames as shared_region_frames, map as map_shared_region,
};
