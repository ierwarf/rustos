pub mod api {
    pub use crate::ipc::{IpcError, KernelSharedRegionHandle};

    pub fn create_shared_region(byte_len: usize) -> Result<KernelSharedRegionHandle, IpcError> {
        crate::ipc::create_shared_region(byte_len)
    }

    pub fn map_shared_region(region: KernelSharedRegionHandle) -> Option<(*mut u8, usize)> {
        crate::ipc::map_shared_region(region)
    }

}
