use rustos_user_abi::syscall::{
    RustosBlockBrokerArgs, BLOCK_BROKER_ABI_VERSION, BLOCK_BROKER_MAX_IO_BYTES,
    BLOCK_BROKER_OP_BOOT_INFO, BLOCK_BROKER_OP_BOOT_READ, SYS_RUSTOS_BLOCK_BROKER,
};
use storage_core::{BlockDevice, IoResult, StorageError};

use super::EINVAL;

pub(super) struct BootBlockDevice {
    pub(super) block_size: usize,
    pub(super) block_count: u64,
}

impl BootBlockDevice {
    pub(super) fn open() -> Result<Self, i32> {
        let mut block_size = 0_u64;
        let mut block_count = 0_u64;
        let args = RustosBlockBrokerArgs {
            abi_version: BLOCK_BROKER_ABI_VERSION,
            op: BLOCK_BROKER_OP_BOOT_INFO,
            out_logical_block_size_ptr: (&mut block_size as *mut u64) as u64,
            out_block_count_ptr: (&mut block_count as *mut u64) as u64,
            ..RustosBlockBrokerArgs::default()
        };
        let status = unsafe {
            rustos_svc_runtime::syscall::syscall1(
                SYS_RUSTOS_BLOCK_BROKER,
                (&args as *const RustosBlockBrokerArgs) as u64,
            )
        };
        if status < 0 {
            return Err((-status) as i32);
        }
        let block_size = usize::try_from(block_size).map_err(|_| EINVAL)?;
        if block_size < 512 || block_count == 0 {
            return Err(EINVAL);
        }
        Ok(Self {
            block_size,
            block_count,
        })
    }
}

impl BlockDevice for BootBlockDevice {
    fn logical_block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        if self.block_size == 0
            || out.len() % self.block_size != 0
            || BLOCK_BROKER_MAX_IO_BYTES < self.block_size
        {
            return Err(StorageError::InvalidInput);
        }
        let max_blocks = (BLOCK_BROKER_MAX_IO_BYTES / self.block_size) as u64;
        let mut done = 0usize;
        while done < out.len() {
            let remaining_blocks = ((out.len() - done) / self.block_size) as u64;
            let block_count = remaining_blocks.min(max_blocks);
            let byte_len = block_count as usize * self.block_size;
            let args = RustosBlockBrokerArgs {
                abi_version: BLOCK_BROKER_ABI_VERSION,
                op: BLOCK_BROKER_OP_BOOT_READ,
                lba: lba + (done / self.block_size) as u64,
                block_count,
                buffer_ptr: out[done..done + byte_len].as_mut_ptr() as u64,
                buffer_len: byte_len as u64,
                ..RustosBlockBrokerArgs::default()
            };
            let status = unsafe {
                rustos_svc_runtime::syscall::syscall1(
                    SYS_RUSTOS_BLOCK_BROKER,
                    (&args as *const RustosBlockBrokerArgs) as u64,
                )
            };
            if status < 0 {
                return Err(StorageError::DeviceFault);
            }
            done += byte_len;
        }
        Ok(())
    }

    fn write_blocks(&mut self, _lba: u64, _input: &[u8]) -> IoResult<()> {
        Err(StorageError::Unsupported)
    }
}
