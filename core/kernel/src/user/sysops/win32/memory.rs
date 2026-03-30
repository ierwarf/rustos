use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use crate::multitask;
use crate::user::abi::UserAbi;
use crate::user::process_state::{WindowsAllocation, WindowsAllocationKind};

use super::constants::{
    MemoryBasicInformation, BOOL_FALSE, BOOL_TRUE, ERROR_INVALID_ADDRESS, ERROR_INVALID_FUNCTION,
    ERROR_INVALID_PARAMETER, ERROR_SUCCESS, MEM_COMMIT, MEM_IMAGE, MEM_PRIVATE, MEM_RELEASE,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_SIZE,
};
use super::runtime::{set_last_error, with_windows_runtime_mut};
use super::util::{address_space_error_to_win32, as_bytes};

pub(crate) fn virtual_query(address: u64, info_ptr: u64, len: u64) -> u64 {
    let result = with_windows_runtime_mut(|process_state, runtime| {
        let out_len = usize::try_from(len).map_err(|_| ERROR_INVALID_PARAMETER)?;
        if info_ptr == 0 || out_len < core::mem::size_of::<MemoryBasicInformation>() {
            return Err(ERROR_INVALID_PARAMETER);
        }
        let region = process_state
            .address_space()
            .regions()
            .iter()
            .copied()
            .find(|region| {
                let start = region.start.as_u64();
                let end = region.end().as_u64();
                address >= start && address < end
            })
            .ok_or(ERROR_INVALID_PARAMETER)?;
        let mut allocation_base = region.start.as_u64();
        let mut allocation_protect = PAGE_READWRITE;
        let mut protect = PAGE_READWRITE;
        let mut region_type = MEM_PRIVATE;
        if let Some(allocation) = process_state.windows_allocation_containing(address, 1) {
            allocation_base = allocation.base;
            allocation_protect = allocation.protect;
            protect = allocation.protect;
        } else if address >= runtime.image_base
            && address < runtime.image_base.saturating_add(runtime.image_size)
        {
            allocation_base = runtime.image_base;
            allocation_protect = PAGE_EXECUTE_READ;
            protect = PAGE_EXECUTE_READ;
            region_type = MEM_IMAGE;
        }
        let info = MemoryBasicInformation {
            base_address: region.start.as_u64(),
            allocation_base,
            allocation_protect,
            partition_id: 0,
            _partition_padding: 0,
            region_size: region.len_bytes() as u64,
            state: MEM_COMMIT as u32,
            protect,
            type_: region_type,
        };
        process_state
            .address_space()
            .validate_user_write_buffer(
                VirtAddr::new(info_ptr),
                core::mem::size_of::<MemoryBasicInformation>(),
            )
            .map_err(address_space_error_to_win32)?;
        process_state
            .address_space()
            .copy_into_user(VirtAddr::new(info_ptr), as_bytes(&info))
            .map_err(address_space_error_to_win32)?;
        Ok(core::mem::size_of::<MemoryBasicInformation>() as u64)
    });

    match result {
        Ok(size) => {
            set_last_error(ERROR_SUCCESS);
            size
        }
        Err(error) => {
            set_last_error(error);
            0
        }
    }
}

pub(crate) fn virtual_alloc(base: u64, len: u64, allocation_type: u64, protect: u64) -> u64 {
    let protect = protect as u32;
    if allocation_type & !(super::constants::MEM_COMMIT | super::constants::MEM_RESERVE) != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if allocation_type & super::constants::MEM_RESERVE == 0
        && allocation_type & super::constants::MEM_COMMIT == 0
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }

    let exact_base = if base == 0 { None } else { Some(base) };
    match allocate_windows_memory(len, protect, exact_base, WindowsAllocationKind::Virtual) {
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

pub(crate) fn virtual_free(base: u64, len: u64, free_type: u64) -> u64 {
    if len != 0 || free_type != MEM_RELEASE {
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
    if old_protect_ptr == 0 {
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

pub(super) fn allocate_windows_memory(
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

pub(super) fn free_windows_memory(
    base: u64,
    expected_kind: Option<WindowsAllocationKind>,
) -> Result<(), u32> {
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

pub(super) fn protect_windows_memory(
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
