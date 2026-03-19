use core::convert::TryFrom;

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::multitask;
use crate::paging;
use crate::user::abi::UserAbi;
use crate::user::process_state::{UserProcessState, WindowsAllocation, WindowsAllocationKind};

use super::console;
use super::usermem;

const STD_INPUT_HANDLE_ID: u32 = 0xffff_fff6;
const STD_OUTPUT_HANDLE_ID: u32 = 0xffff_fff5;
const STD_ERROR_HANDLE_ID: u32 = 0xffff_fff4;

const HANDLE_STDIN: u64 = 0x1000_0001;
const HANDLE_STDOUT: u64 = 0x1000_0002;
const HANDLE_STDERR: u64 = 0x1000_0003;
const HANDLE_PROCESS_HEAP: u64 = 0x1000_0010;

const BOOL_FALSE: u64 = 0;
const BOOL_TRUE: u64 = 1;
const INVALID_HANDLE_VALUE: u64 = u64::MAX;
const PAGE_SIZE: u64 = 4096;

const FILE_TYPE_CHAR: u64 = 0x0002;
const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;

const PAGE_NOACCESS: u32 = 0x0001;
const PAGE_READONLY: u32 = 0x0002;
const PAGE_READWRITE: u32 = 0x0004;
const PAGE_EXECUTE_READ: u32 = 0x0020;
const PAGE_EXECUTE_READWRITE: u32 = 0x0040;
const MEM_COMMIT: u64 = 0x1000;
const MEM_RESERVE: u64 = 0x2000;
const MEM_RELEASE: u64 = 0x8000;
const HEAP_NO_SERIALIZE: u64 = 0x0000_0001;
const HEAP_GENERATE_EXCEPTIONS: u64 = 0x0000_0004;
const HEAP_ZERO_MEMORY: u64 = 0x0000_0008;

pub(crate) const ERROR_SUCCESS: u32 = 0;
pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
pub(crate) const ERROR_INVALID_HANDLE: u32 = 6;
pub(crate) const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
pub(crate) const ERROR_NOT_READY: u32 = 21;
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
pub(crate) const ERROR_INVALID_ADDRESS: u32 = 487;

pub(crate) fn exit_process(_exit_code: u64) -> u64 {
    multitask::exit_current_user_task()
}

pub(crate) fn get_std_handle(std_handle: u64) -> u64 {
    set_last_error(ERROR_SUCCESS);
    match std_handle as u32 {
        STD_INPUT_HANDLE_ID => HANDLE_STDIN,
        STD_OUTPUT_HANDLE_ID => HANDLE_STDOUT,
        STD_ERROR_HANDLE_ID => HANDLE_STDERR,
        _ => {
            set_last_error(ERROR_INVALID_PARAMETER);
            INVALID_HANDLE_VALUE
        }
    }
}

pub(crate) fn write_file(
    handle: u64,
    buffer: u64,
    len: u64,
    bytes_written_ptr: u64,
    overlapped: u64,
) -> u64 {
    if overlapped != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    if !matches!(handle, HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_INVALID_HANDLE);
        return BOOL_FALSE;
    }

    let Ok(len) = usize::try_from(len) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    };

    let Some(address_space) = multitask::current_user_address_space() else {
        set_last_error(ERROR_INVALID_FUNCTION);
        return BOOL_FALSE;
    };

    if bytes_written_ptr != 0
        && address_space
            .validate_user_write_buffer(VirtAddr::new(bytes_written_ptr), 4)
            .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    match console::write_from_current_process(buffer, len) {
        Ok(written) => {
            if bytes_written_ptr != 0
                && usermem::write_current_user_u32(bytes_written_ptr, written as u32).is_err()
            {
                set_last_error(ERROR_INVALID_PARAMETER);
                return BOOL_FALSE;
            }
            set_last_error(ERROR_SUCCESS);
            BOOL_TRUE
        }
        Err(err) => {
            set_last_error(address_space_error_to_win32(err));
            BOOL_FALSE
        }
    }
}

pub(crate) fn read_file(
    handle: u64,
    buffer: u64,
    len: u64,
    bytes_read_ptr: u64,
    overlapped: u64,
) -> u64 {
    if overlapped != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    if handle != HANDLE_STDIN {
        set_last_error(ERROR_INVALID_HANDLE);
        return BOOL_FALSE;
    }

    let Ok(len) = usize::try_from(len) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    };

    let Some(address_space) = multitask::current_user_address_space() else {
        set_last_error(ERROR_INVALID_FUNCTION);
        return BOOL_FALSE;
    };

    if address_space
        .validate_user_write_buffer(VirtAddr::new(buffer), len)
        .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    if bytes_read_ptr != 0
        && address_space
            .validate_user_write_buffer(VirtAddr::new(bytes_read_ptr), 4)
            .is_err()
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    match console::read_into_current_process(buffer, len) {
        Ok(read) => {
            if bytes_read_ptr != 0
                && usermem::write_current_user_u32(bytes_read_ptr, read as u32).is_err()
            {
                set_last_error(ERROR_INVALID_PARAMETER);
                return BOOL_FALSE;
            }
            set_last_error(ERROR_SUCCESS);
            BOOL_TRUE
        }
        Err(err) => {
            set_last_error(address_space_error_to_win32(err));
            BOOL_FALSE
        }
    }
}

pub(crate) fn sleep(milliseconds: u64) -> u64 {
    crate::rtc::sleep(milliseconds);
    set_last_error(ERROR_SUCCESS);
    0
}

pub(crate) fn close_handle(handle: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return BOOL_TRUE;
    }

    set_last_error(ERROR_INVALID_HANDLE);
    BOOL_FALSE
}

pub(crate) fn get_last_error() -> u64 {
    multitask::current_last_error() as u64
}

pub(crate) fn set_last_error(value: u32) -> u64 {
    multitask::set_current_last_error(value);
    0
}

pub(crate) fn get_file_type(handle: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return FILE_TYPE_CHAR;
    }

    set_last_error(ERROR_INVALID_HANDLE);
    0
}

pub(crate) fn get_console_mode(handle: u64, mode_ptr: u64) -> u64 {
    let mode = match handle {
        HANDLE_STDIN => ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT,
        HANDLE_STDOUT | HANDLE_STDERR => ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT,
        _ => {
            set_last_error(ERROR_INVALID_HANDLE);
            return BOOL_FALSE;
        }
    };

    if mode_ptr == 0 || usermem::write_current_user_u32(mode_ptr, mode).is_err() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    set_last_error(ERROR_SUCCESS);
    BOOL_TRUE
}

pub(crate) fn set_console_mode(handle: u64, _mode: u64) -> u64 {
    if matches!(handle, HANDLE_STDIN | HANDLE_STDOUT | HANDLE_STDERR) {
        set_last_error(ERROR_SUCCESS);
        return BOOL_TRUE;
    }

    set_last_error(ERROR_INVALID_HANDLE);
    BOOL_FALSE
}

pub(crate) fn get_process_heap() -> u64 {
    set_last_error(ERROR_SUCCESS);
    HANDLE_PROCESS_HEAP
}

pub(crate) fn heap_alloc(heap: u64, flags: u64, len: u64) -> u64 {
    if heap != HANDLE_PROCESS_HEAP {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if flags & !(HEAP_NO_SERIALIZE | HEAP_ZERO_MEMORY) != 0
        || flags & HEAP_GENERATE_EXCEPTIONS != 0
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }

    match allocate_windows_memory(
        len.max(1),
        PAGE_READWRITE,
        None,
        WindowsAllocationKind::Heap,
    ) {
        Ok(base) => {
            set_last_error(ERROR_SUCCESS);
            base
        }
        Err(error) => {
            set_last_error(error);
            0
        }
    }
}

pub(crate) fn heap_free(heap: u64, flags: u64, base: u64) -> u64 {
    if heap != HANDLE_PROCESS_HEAP {
        set_last_error(ERROR_INVALID_HANDLE);
        return BOOL_FALSE;
    }
    if flags & !HEAP_NO_SERIALIZE != 0 || base == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    match free_windows_memory(base, Some(WindowsAllocationKind::Heap)) {
        Ok(()) => {
            set_last_error(ERROR_SUCCESS);
            BOOL_TRUE
        }
        Err(error) => {
            set_last_error(error);
            BOOL_FALSE
        }
    }
}

pub(crate) fn heap_realloc(heap: u64, flags: u64, base: u64, len: u64) -> u64 {
    if heap != HANDLE_PROCESS_HEAP {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if flags & !(HEAP_NO_SERIALIZE | HEAP_ZERO_MEMORY) != 0
        || flags & HEAP_GENERATE_EXCEPTIONS != 0
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if base == 0 {
        return heap_alloc(heap, flags, len);
    }

    match reallocate_heap_block(base, len.max(1)) {
        Ok(ptr) => {
            set_last_error(ERROR_SUCCESS);
            ptr
        }
        Err(error) => {
            set_last_error(error);
            0
        }
    }
}

pub(crate) fn virtual_alloc(base: u64, len: u64, allocation_type: u64, protect: u64) -> u64 {
    if len == 0 || allocation_type & (MEM_COMMIT | MEM_RESERVE) == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if allocation_type & !(MEM_COMMIT | MEM_RESERVE) != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }

    let exact_base = if base == 0 { None } else { Some(base) };
    match allocate_windows_memory(len, protect as u32, exact_base, WindowsAllocationKind::Virtual) {
        Ok(addr) => {
            set_last_error(ERROR_SUCCESS);
            addr
        }
        Err(error) => {
            set_last_error(error);
            0
        }
    }
}

pub(crate) fn virtual_free(base: u64, len: u64, free_type: u64) -> u64 {
    if base == 0 || free_type != MEM_RELEASE || len != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    match free_windows_memory(base, Some(WindowsAllocationKind::Virtual)) {
        Ok(()) => {
            set_last_error(ERROR_SUCCESS);
            BOOL_TRUE
        }
        Err(error) => {
            set_last_error(error);
            BOOL_FALSE
        }
    }
}

pub(crate) fn virtual_protect(base: u64, len: u64, protect: u64, old_protect_ptr: u64) -> u64 {
    if base == 0 || len == 0 || old_protect_ptr == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return BOOL_FALSE;
    }

    match protect_windows_memory(base, len, protect as u32, old_protect_ptr) {
        Ok(()) => {
            set_last_error(ERROR_SUCCESS);
            BOOL_TRUE
        }
        Err(error) => {
            set_last_error(error);
            BOOL_FALSE
        }
    }
}

fn address_space_error_to_win32(err: paging::AddressSpaceError) -> u32 {
    match err {
        paging::AddressSpaceError::ZeroSizedAllocation => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AddressOverflow => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AddressOutOfRange => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AddressNotPageAligned => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::AlreadyMapped => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::NotMapped => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::ProtectionViolation => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::HugePageConflict => ERROR_INVALID_PARAMETER,
        paging::AddressSpaceError::OutOfFrames => ERROR_NOT_ENOUGH_MEMORY,
    }
}

fn allocate_windows_memory(
    len: u64,
    protect: u32,
    exact_base: Option<u64>,
    kind: WindowsAllocationKind,
) -> Result<u64, u32> {
    let page_flags = page_flags_from_win32_protect(protect)?;
    let (page_count, mapped_len) = normalized_mapping_size(len).ok_or(ERROR_INVALID_PARAMETER)?;

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Windows {
            return Err(ERROR_INVALID_FUNCTION);
        }

        let region = if let Some(base) = exact_base {
            if base & (PAGE_SIZE - 1) != 0 {
                return Err(ERROR_INVALID_PARAMETER);
            }
            process_state
                .address_space_mut()
                .map_zeroed_user_pages_at(VirtAddr::new(base), page_count, page_flags)
                .map_err(address_space_error_to_win32)?
        } else {
            process_state
                .map_zeroed_pages_from_mapping_cursor(page_count, page_flags)
                .map_err(address_space_error_to_win32)?
        };

        process_state.record_windows_allocation(WindowsAllocation::new(
            region.start.as_u64(),
            mapped_len,
            protect,
            kind,
        ));
        Ok(region.start.as_u64())
    }) else {
        return Err(ERROR_INVALID_FUNCTION);
    };

    result
}

fn free_windows_memory(base: u64, expected_kind: Option<WindowsAllocationKind>) -> Result<(), u32> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Windows {
            return Err(ERROR_INVALID_FUNCTION);
        }

        let allocation = process_state
            .windows_allocation(base)
            .ok_or(ERROR_INVALID_ADDRESS)?;
        if expected_kind.is_some() && Some(allocation.kind) != expected_kind {
            return Err(ERROR_INVALID_ADDRESS);
        }

        process_state
            .address_space_mut()
            .unmap_user_bytes(
                VirtAddr::new(allocation.base),
                usize::try_from(allocation.len).map_err(|_| ERROR_INVALID_PARAMETER)?,
            )
            .map_err(address_space_error_to_win32)?;
        process_state.remove_windows_allocation(base);
        Ok(())
    }) else {
        return Err(ERROR_INVALID_FUNCTION);
    };

    result
}

fn protect_windows_memory(
    base: u64,
    len: u64,
    protect: u32,
    old_protect_ptr: u64,
) -> Result<(), u32> {
    let page_flags = page_flags_from_win32_protect(protect)?;

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Windows {
            return Err(ERROR_INVALID_FUNCTION);
        }

        let allocation = process_state
            .windows_allocation_containing(base, len)
            .ok_or(ERROR_INVALID_ADDRESS)?;
        if allocation.base != base || allocation.len != len {
            return Err(ERROR_INVALID_PARAMETER);
        }
        process_state
            .address_space()
            .validate_user_write_buffer(VirtAddr::new(old_protect_ptr), 4)
            .map_err(address_space_error_to_win32)?;

        let old_protect = allocation.protect;
        process_state
            .address_space_mut()
            .protect_user_bytes(
                VirtAddr::new(base),
                usize::try_from(len).map_err(|_| ERROR_INVALID_PARAMETER)?,
                page_flags,
            )
            .map_err(address_space_error_to_win32)?;
        process_state
            .address_space()
            .copy_into_user(VirtAddr::new(old_protect_ptr), &old_protect.to_le_bytes())
            .map_err(address_space_error_to_win32)?;
        process_state
            .update_windows_allocation_protect(base, protect)
            .ok_or(ERROR_INVALID_ADDRESS)?;
        Ok(())
    }) else {
        return Err(ERROR_INVALID_FUNCTION);
    };

    result
}

fn reallocate_heap_block(base: u64, new_len: u64) -> Result<u64, u32> {
    let (page_count, mapped_len) = normalized_mapping_size(new_len).ok_or(ERROR_INVALID_PARAMETER)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Windows {
            return Err(ERROR_INVALID_FUNCTION);
        }

        let allocation = process_state
            .windows_allocation(base)
            .ok_or(ERROR_INVALID_ADDRESS)?;
        if allocation.kind != WindowsAllocationKind::Heap {
            return Err(ERROR_INVALID_ADDRESS);
        }

        if mapped_len <= allocation.len {
            return Ok(base);
        }

        let new_region = process_state
            .map_zeroed_pages_from_mapping_cursor(page_count, PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE)
            .map_err(address_space_error_to_win32)?;
        let new_base = new_region.start.as_u64();

        copy_user_region(
            process_state,
            allocation.base,
            new_base,
            usize::try_from(allocation.len).map_err(|_| ERROR_INVALID_PARAMETER)?,
        )?;

        process_state
            .address_space_mut()
            .unmap_user_bytes(
                VirtAddr::new(allocation.base),
                usize::try_from(allocation.len).map_err(|_| ERROR_INVALID_PARAMETER)?,
            )
            .map_err(address_space_error_to_win32)?;
        process_state.remove_windows_allocation(allocation.base);
        process_state.record_windows_allocation(WindowsAllocation::new(
            new_base,
            mapped_len,
            allocation.protect,
            WindowsAllocationKind::Heap,
        ));
        Ok(new_base)
    }) else {
        return Err(ERROR_INVALID_FUNCTION);
    };

    result
}

fn copy_user_region(
    process_state: &mut UserProcessState,
    src: u64,
    dst: u64,
    len: usize,
) -> Result<(), u32> {
    const COPY_CHUNK_LEN: usize = 512;

    let address_space = process_state.address_space();
    let mut copied = 0usize;
    let mut chunk = [0_u8; COPY_CHUNK_LEN];
    while copied < len {
        let chunk_len = (len - copied).min(chunk.len());
        address_space
            .copy_from_user(
                VirtAddr::new(src.checked_add(copied as u64).ok_or(ERROR_INVALID_PARAMETER)?),
                &mut chunk[..chunk_len],
            )
            .map_err(address_space_error_to_win32)?;
        address_space
            .initialize_user_bytes(
                VirtAddr::new(dst.checked_add(copied as u64).ok_or(ERROR_INVALID_PARAMETER)?),
                &chunk[..chunk_len],
            )
            .map_err(address_space_error_to_win32)?;
        copied += chunk_len;
    }
    Ok(())
}

fn page_flags_from_win32_protect(protect: u32) -> Result<PageTableFlags, u32> {
    let flags = match protect {
        PAGE_READONLY => PageTableFlags::NO_EXECUTE,
        PAGE_READWRITE => PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        PAGE_EXECUTE_READ => PageTableFlags::empty(),
        PAGE_EXECUTE_READWRITE => PageTableFlags::WRITABLE,
        PAGE_NOACCESS => return Err(ERROR_INVALID_PARAMETER),
        _ => return Err(ERROR_INVALID_PARAMETER),
    };
    Ok(flags)
}

fn normalized_mapping_size(len: u64) -> Option<(usize, u64)> {
    let aligned_len = align_up_u64(len.max(1), PAGE_SIZE)?;
    let page_count = usize::try_from(aligned_len / PAGE_SIZE).ok()?;
    Some((page_count, aligned_len))
}

fn align_up_u64(value: u64, align: u64) -> Option<u64> {
    if align <= 1 {
        return Some(value);
    }
    let mask = align.checked_sub(1)?;
    value.checked_add(mask).map(|value| value & !mask)
}
