use super::*;

pub(crate) fn mmap(
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> Result<u64, LinuxSysopError> {
    let supported_prot = linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC;
    if prot & !supported_prot != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let fixed_mapping = flags & linux_abi::MAP_FIXED != 0;
    if fixed_mapping && (requested_addr == 0 || requested_addr & (PAGE_SIZE - 1) != 0) {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    if !linux_mmap_fd_is_anonymous(fd) {
        if let Some(mapped_addr) =
            mmap_current_process_file(fd, requested_addr, user_len, prot, flags, offset)?
        {
            return Ok(mapped_addr);
        }

        if requested_addr != 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        return device::mmap_current_process_handle(fd, user_len, prot, flags, offset)
            .map_err(Into::into);
    }

    if offset != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if flags & linux_abi::MAP_PRIVATE == 0 || flags & linux_abi::MAP_ANONYMOUS == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let page_count = len.div_ceil(PAGE_SIZE as usize);
    let page_flags = linux_mmap_page_flags(prot);

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let mapped_addr = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            let Some(state) = linux_process_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };

            if prot == 0 {
                reserve_linux_user_region(
                    address_space,
                    state,
                    requested_addr,
                    fixed_mapping,
                    page_count,
                )?
                .start
                .as_u64()
            } else {
                map_linux_user_region(
                    address_space,
                    state,
                    requested_addr,
                    fixed_mapping,
                    page_count,
                    page_flags,
                )?
                .start
                .as_u64()
            }
        };
        let mapped_end = mapped_addr
            .checked_add((page_count as u64).saturating_mul(PAGE_SIZE))
            .ok_or(LinuxSysopError::NoMemory)?;
        process_state.set_mapping_cursor(mapped_end);
        Ok(mapped_addr)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn linux_mmap_fd_is_anonymous(fd: u64) -> bool {
    fd == u64::MAX || fd == u32::MAX as u64
}

pub(crate) fn munmap(start: u64, user_len: u64) -> Result<(), LinuxSysopError> {
    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let end = start
        .checked_add(len as u64)
        .ok_or(LinuxSysopError::InvalidArgument)?;

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let unmapped_len = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            match address_space.unmap_user_bytes(VirtAddr::new(start), len) {
                Ok(unmapped_pages) => Some(
                    (unmapped_pages as u64)
                        .checked_mul(PAGE_SIZE)
                        .ok_or(LinuxSysopError::InvalidArgument)?,
                ),
                Err(paging::AddressSpaceError::NotMapped) => {
                    let Some(state) = linux_process_state.as_mut() else {
                        return Err(LinuxSysopError::Unsupported);
                    };
                    state.release_reserved_range(start, end).map_err(|_| {
                        LinuxSysopError::AddressSpace(paging::AddressSpaceError::NotMapped)
                    })?;
                    None
                }
                Err(err) => return Err(LinuxSysopError::AddressSpace(err)),
            }
        };

        if let Some(unmapped_len) = unmapped_len {
            process_state
                .handles_mut()
                .clear_surface_mappings_in_range(start, unmapped_len);
        }
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn mprotect(start: u64, user_len: u64, prot: u64) -> Result<(), LinuxSysopError> {
    let supported_prot = linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC;
    if prot & !supported_prot != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(());
    }
    let page_flags = linux_mmap_page_flags(prot);

    let Some(result) = multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, linux_process_state, _| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }

            match address_space.protect_user_bytes(VirtAddr::new(start), len, page_flags) {
                Ok(()) => Ok(()),
                Err(paging::AddressSpaceError::NotMapped) => {
                    let end = start
                        .checked_add(len as u64)
                        .ok_or(LinuxSysopError::InvalidArgument)?;
                    let Some(state) = linux_process_state.as_mut() else {
                        return Err(LinuxSysopError::Unsupported);
                    };
                    if !state.is_range_reserved(start, end) {
                        return Err(LinuxSysopError::AddressSpace(
                            paging::AddressSpaceError::NotMapped,
                        ));
                    }
                    address_space
                        .map_zeroed_user_bytes_at(VirtAddr::new(start), len, page_flags)
                        .map_err(LinuxSysopError::AddressSpace)?;
                    state.release_reserved_range(start, end).map_err(|_| {
                        LinuxSysopError::AddressSpace(paging::AddressSpaceError::NotMapped)
                    })
                }
                Err(err) => Err(LinuxSysopError::AddressSpace(err)),
            }
        },
    ) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn brk(addr: u64) -> u64 {
    let Some(result) = multitask::with_current_user_linux_state_mut(
        |_, _, abi, address_space, linux_process_state, _| {
            if abi != UserAbi::Linux {
                return 0;
            }

            let Some(state) = linux_process_state.as_mut() else {
                return 0;
            };
            if addr == 0 {
                return state.brk_current;
            }
            if addr < state.brk_start {
                return state.brk_current;
            }

            let requested_mapped_end = align_up(addr, PAGE_SIZE);
            if !state.can_grow_brk_to(requested_mapped_end) {
                return state.brk_current;
            }

            if requested_mapped_end > state.brk_mapped_end {
                let delta = requested_mapped_end - state.brk_mapped_end;
                let page_count = (delta / PAGE_SIZE) as usize;
                let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
                if address_space
                    .map_zeroed_user_pages_at(
                        VirtAddr::new(state.brk_mapped_end),
                        page_count,
                        flags,
                    )
                    .is_err()
                {
                    return state.brk_current;
                }
                state.brk_mapped_end = requested_mapped_end;
            }

            state.brk_current = addr;
            addr
        },
    ) else {
        return 0;
    };

    result
}

fn linux_mmap_page_flags(prot: u64) -> PageTableFlags {
    let mut flags = PageTableFlags::empty();
    if prot & linux_abi::PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if prot & linux_abi::PROT_EXEC == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value.saturating_add(align - 1) & !(align - 1)
}

fn map_linux_user_region(
    address_space: &mut paging::ProcessAddressSpace,
    linux_process_state: &mut linux_abi::LinuxProcessState,
    requested_addr: u64,
    fixed_mapping: bool,
    page_count: usize,
    page_flags: PageTableFlags,
) -> Result<crate::paging::UserRegion, LinuxSysopError> {
    let span = (page_count as u64)
        .checked_mul(PAGE_SIZE)
        .ok_or(LinuxSysopError::NoMemory)?;
    let default_start = align_up(linux_process_state.mmap_next, PAGE_SIZE);

    if fixed_mapping {
        return map_linux_user_region_at(
            address_space,
            linux_process_state,
            requested_addr,
            span,
            page_count,
            page_flags,
            true,
        );
    }

    if requested_addr != 0 {
        let hinted_start = align_up(requested_addr, PAGE_SIZE);
        if let Ok(region) = map_linux_user_region_at(
            address_space,
            linux_process_state,
            hinted_start,
            span,
            page_count,
            page_flags,
            false,
        ) {
            return Ok(region);
        }
    }

    map_linux_user_region_at(
        address_space,
        linux_process_state,
        default_start,
        span,
        page_count,
        page_flags,
        false,
    )
}

fn reserve_linux_user_region(
    address_space: &paging::ProcessAddressSpace,
    linux_process_state: &mut linux_abi::LinuxProcessState,
    requested_addr: u64,
    fixed_mapping: bool,
    page_count: usize,
) -> Result<crate::paging::UserRegion, LinuxSysopError> {
    let span = (page_count as u64)
        .checked_mul(PAGE_SIZE)
        .ok_or(LinuxSysopError::NoMemory)?;
    let default_start = align_up(linux_process_state.mmap_next, PAGE_SIZE);

    if fixed_mapping {
        return reserve_linux_user_region_at(
            address_space,
            linux_process_state,
            requested_addr,
            span,
            page_count,
        );
    }

    if requested_addr != 0 {
        let hinted_start = align_up(requested_addr, PAGE_SIZE);
        if let Ok(region) = reserve_linux_user_region_at(
            address_space,
            linux_process_state,
            hinted_start,
            span,
            page_count,
        ) {
            return Ok(region);
        }
    }

    reserve_linux_user_region_at(
        address_space,
        linux_process_state,
        default_start,
        span,
        page_count,
    )
}

fn map_linux_user_region_at(
    address_space: &mut paging::ProcessAddressSpace,
    linux_process_state: &mut linux_abi::LinuxProcessState,
    start: u64,
    span: u64,
    page_count: usize,
    page_flags: PageTableFlags,
    replace_existing: bool,
) -> Result<crate::paging::UserRegion, LinuxSysopError> {
    let end = start.checked_add(span).ok_or(LinuxSysopError::NoMemory)?;
    if end > linux_process_state.brk_limit() || end <= linux_process_state.brk_mapped_end {
        return Err(LinuxSysopError::NoMemory);
    }

    if linux_process_state.has_reserved_overlap(start, end) {
        if !replace_existing {
            return Err(LinuxSysopError::NoMemory);
        }
        linux_process_state
            .release_reserved_range(start, end)
            .map_err(|_| LinuxSysopError::NoMemory)?;
    }

    if replace_existing {
        match address_space.unmap_user_bytes(
            VirtAddr::new(start),
            usize::try_from(span).map_err(|_| LinuxSysopError::InvalidArgument)?,
        ) {
            Ok(_) | Err(paging::AddressSpaceError::NotMapped) => {}
            Err(err) => return Err(LinuxSysopError::AddressSpace(err)),
        }
    }

    let region = address_space
        .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, page_flags)
        .map_err(LinuxSysopError::AddressSpace)?;
    if region.end().as_u64() > linux_process_state.mmap_next {
        linux_process_state.mmap_next = align_up(region.end().as_u64(), PAGE_SIZE);
    }
    Ok(region)
}

fn reserve_linux_user_region_at(
    address_space: &paging::ProcessAddressSpace,
    linux_process_state: &mut linux_abi::LinuxProcessState,
    start: u64,
    span: u64,
    page_count: usize,
) -> Result<crate::paging::UserRegion, LinuxSysopError> {
    let end = start.checked_add(span).ok_or(LinuxSysopError::NoMemory)?;
    if end > linux_process_state.brk_limit() || end <= linux_process_state.brk_mapped_end {
        return Err(LinuxSysopError::NoMemory);
    }

    if address_space
        .regions()
        .iter()
        .any(|region| region.start.as_u64() < end && start < region.end().as_u64())
    {
        return Err(LinuxSysopError::NoMemory);
    }
    if linux_process_state.has_reserved_overlap(start, end) {
        return Err(LinuxSysopError::NoMemory);
    }

    linux_process_state
        .reserve_range(start, end)
        .map_err(|_| LinuxSysopError::NoMemory)?;
    if end > linux_process_state.mmap_next {
        linux_process_state.mmap_next = align_up(end, PAGE_SIZE);
    }
    Ok(crate::paging::UserRegion {
        start: VirtAddr::new(start),
        page_count,
    })
}

fn mmap_current_process_file(
    fd: u64,
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    offset: u64,
) -> Result<Option<u64>, LinuxSysopError> {
    let file_map_len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let page_count = usize::try_from(user_len.div_ceil(PAGE_SIZE))
        .map_err(|_| LinuxSysopError::InvalidArgument)?;
    let fixed_mapping = flags & linux_abi::MAP_FIXED != 0;

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        if !matches!(
            process_state.handles().get(fd),
            Some(KernelHandle::BootFile(_))
        ) {
            return Ok(None);
        }
        if offset & (PAGE_SIZE - 1) != 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        if flags & linux_abi::MAP_ANONYMOUS != 0 || flags & linux_abi::MAP_PRIVATE == 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        if fixed_mapping && (requested_addr == 0 || requested_addr & (PAGE_SIZE - 1) != 0) {
            return Err(LinuxSysopError::InvalidArgument);
        }

        let page_flags = linux_mmap_page_flags(prot);
        let file_offset = usize::try_from(offset).map_err(|_| LinuxSysopError::InvalidArgument)?;
        let region = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            let Some(state) = linux_process_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };
            map_linux_user_region(
                address_space,
                state,
                requested_addr,
                fixed_mapping,
                page_count,
                page_flags,
            )?
        };
        process_state.set_mapping_cursor(region.end().as_u64());

        let (file_len, _file_path) = match process_state.handles().get(fd) {
            Some(KernelHandle::BootFile(file)) => (file.len(), file.path()),
            Some(_) => return Ok(None),
            None => return Err(LinuxSysopError::BadFileDescriptor),
        };

        if file_offset < file_len {
            let copy_len = file_map_len.min(file_len - file_offset);
            let mut copied = 0usize;
            let mut chunk = [0_u8; FILE_MMAP_COPY_CHUNK_LEN];
            while copied < copy_len {
                let chunk_len = (copy_len - copied).min(chunk.len());
                let read = {
                    let Some(KernelHandle::BootFile(file)) =
                        process_state.handles_mut().get_mut(fd)
                    else {
                        return Err(LinuxSysopError::BadFileDescriptor);
                    };
                    file.read_at(file_offset + copied, &mut chunk[..chunk_len])
                };
                if read == 0 {
                    break;
                }

                let chunk_ptr = region
                    .start
                    .as_u64()
                    .checked_add(copied as u64)
                    .ok_or(LinuxSysopError::InvalidArgument)?;
                process_state
                    .address_space()
                    .initialize_user_bytes(VirtAddr::new(chunk_ptr), &chunk[..read])
                    .map_err(LinuxSysopError::AddressSpace)?;
                copied += read;
            }
        }

        Ok(Some(region.start.as_u64()))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}
