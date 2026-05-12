// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// pub(super) const HANDLE_STDIN: u64 = 0x1000_0001;
// pub(super) const HANDLE_STDOUT: u64 = 0x1000_0002;
// pub(super) const HANDLE_STDERR: u64 = 0x1000_0003;
// pub(crate) const HANDLE_PROCESS_HEAP: u64 = 0x1000_0010;
// pub(super) const HANDLE_CURRENT_PROCESS: u64 = u64::MAX;
// 
// pub(super) const BOOL_FALSE: u64 = 0;
// pub(super) const BOOL_TRUE: u64 = 1;
// pub(super) const PAGE_SIZE: u64 = 4096;
// 
// pub(super) const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
// pub(super) const ENABLE_LINE_INPUT: u32 = 0x0002;
// pub(super) const ENABLE_ECHO_INPUT: u32 = 0x0004;
// pub(super) const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
// pub(super) const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
// 
// pub(super) const PAGE_NOACCESS: u32 = 0x0001;
// pub(super) const PAGE_READONLY: u32 = 0x0002;
// pub(super) const PAGE_READWRITE: u32 = 0x0004;
// pub(super) const PAGE_EXECUTE_READ: u32 = 0x0020;
// pub(super) const PAGE_EXECUTE_READWRITE: u32 = 0x0040;
// pub(super) const MEM_COMMIT: u64 = 0x1000;
// pub(super) const MEM_RESERVE: u64 = 0x2000;
// pub(super) const MEM_RELEASE: u64 = 0x8000;
// pub(super) const MEM_PRIVATE: u32 = 0x20000;
// pub(super) const MEM_IMAGE: u32 = 0x1000000;
// 
// pub(crate) const ERROR_SUCCESS: u32 = 0;
// pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
// pub(super) const ERROR_INVALID_HANDLE: u32 = 6;
// pub(super) const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
// pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
// pub(super) const ERROR_INVALID_ADDRESS: u32 = 487;
// 
// #[repr(C)]
// #[derive(Clone, Copy, Default)]
// pub(super) struct MemoryBasicInformation {
//     pub(super) base_address: u64,
//     pub(super) allocation_base: u64,
//     pub(super) allocation_protect: u32,
//     pub(super) partition_id: u16,
//     pub(super) _partition_padding: u16,
//     pub(super) region_size: u64,
//     pub(super) state: u32,
//     pub(super) protect: u32,
//     pub(super) type_: u32,
// }
