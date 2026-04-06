use super::*;
use alloc::string::String;
use alloc::vec::Vec;

pub(crate) fn mmap(
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> Result<u64, LinuxSysopError> {
    let flags = linux_mmap_effective_flags(flags);
    validate_linux_protection(prot, true)?;
    let fixed_mapping = flags & linux_abi::MAP_FIXED != 0;
    if fixed_mapping && (requested_addr == 0 || requested_addr & (PAGE_SIZE - 1) != 0) {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    if !linux_mmap_fd_is_anonymous(fd) {
        return match current_process_mmap_handle_kind(fd)? {
            LinuxMmapHandleKind::Memfd => {
                mmap_current_process_memfd(fd, requested_addr, user_len, prot, flags, offset)?
                    .ok_or(LinuxSysopError::BadFileDescriptor)
            }
            LinuxMmapHandleKind::File => {
                mmap_current_process_file(fd, requested_addr, user_len, prot, flags, offset)?
                    .ok_or(LinuxSysopError::BadFileDescriptor)
            }
            LinuxMmapHandleKind::Device => {
                mmap_current_process_device(fd, requested_addr, user_len, prot, flags, offset)
            }
        };
    }

    if offset != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if !linux_mmap_is_private(flags) || flags & linux_abi::MAP_ANONYMOUS == 0 {
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
        let area = linux_abi::LinuxVma::new(
            mapped_addr,
            mapped_end,
            0,
            linux_vma_flags_from_mmap(prot, flags),
            linux_abi::LinuxVmaName::None,
        )
        .ok_or(LinuxSysopError::InvalidArgument)?;
        let memory_map = process_state
            .linux_memory_map_mut()
            .ok_or(LinuxSysopError::Unsupported)?;
        if fixed_mapping {
            memory_map.replace_area(area);
        } else {
            memory_map
                .insert_area(area)
                .map_err(|_| LinuxSysopError::NoMemory)?;
        }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinuxMmapHandleKind {
    Memfd,
    File,
    Device,
}

fn current_process_mmap_handle_kind(fd: u64) -> Result<LinuxMmapHandleKind, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_state(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        match process_state.handles().get(fd) {
            Some(KernelHandle::Memfd(_)) => Ok(LinuxMmapHandleKind::Memfd),
            Some(KernelHandle::VfsFile(_)) => Ok(LinuxMmapHandleKind::File),
            Some(KernelHandle::Device(_) | KernelHandle::DisplaySurface(_)) => {
                Ok(LinuxMmapHandleKind::Device)
            }
            Some(_) => Err(LinuxSysopError::BadFileDescriptor),
            None => Err(LinuxSysopError::BadFileDescriptor),
        }
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn linux_mmap_effective_flags(flags: u64) -> u64 {
    // glibc still passes legacy bookkeeping flags such as MAP_DENYWRITE and
    // MAP_EXECUTABLE for ELF segments; Linux ignores them for mmap semantics.
    flags & !(linux_abi::MAP_DENYWRITE | linux_abi::MAP_EXECUTABLE)
}

fn linux_mmap_mapping_type(flags: u64) -> u64 {
    flags & linux_abi::MAP_TYPE
}

fn linux_mmap_is_shared(flags: u64) -> bool {
    matches!(
        linux_mmap_mapping_type(flags),
        linux_abi::MAP_SHARED | linux_abi::MAP_SHARED_VALIDATE
    )
}

fn linux_mmap_is_private(flags: u64) -> bool {
    linux_mmap_mapping_type(flags) == linux_abi::MAP_PRIVATE
}

fn mmap_current_process_device(
    fd: u64,
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    offset: u64,
) -> Result<u64, LinuxSysopError> {
    if requested_addr != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let mapped_addr = device::mmap_current_process_handle(fd, user_len, prot, flags, offset)
        .map_err(LinuxSysopError::from)?;
    let record_result = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let mapping_name = match process_state.handles().get(fd) {
            Some(KernelHandle::DisplaySurface(_)) => {
                linux_abi::LinuxVmaName::Label("anon_inode:[rustos-display-surface]")
            }
            Some(KernelHandle::Device(device)) => linux_abi::LinuxVmaName::Path(
                alloc::string::String::from(device.device_id().path()),
            ),
            Some(_) => return Err(LinuxSysopError::BadFileDescriptor),
            None => return Err(LinuxSysopError::BadFileDescriptor),
        };
        let mapped_end = mapped_addr
            .checked_add(align_up(user_len, PAGE_SIZE))
            .ok_or(LinuxSysopError::InvalidArgument)?;
        let area = linux_abi::LinuxVma::new(
            mapped_addr,
            mapped_end,
            offset,
            linux_vma_flags_from_mmap(prot, flags),
            mapping_name,
        )
        .ok_or(LinuxSysopError::InvalidArgument)?;
        let memory_map = process_state
            .linux_memory_map_mut()
            .ok_or(LinuxSysopError::Unsupported)?;
        memory_map.replace_area(area);
        Ok(())
    });

    let Some(record_result) = record_result else {
        let _ = device::munmap_current_process_range(mapped_addr, user_len);
        return Err(LinuxSysopError::Unsupported);
    };
    if let Err(err) = record_result {
        let _ = device::munmap_current_process_range(mapped_addr, user_len);
        return Err(err);
    }

    Ok(mapped_addr)
}

pub(crate) fn munmap(start: u64, user_len: u64) -> Result<(), LinuxSysopError> {
    if start & (PAGE_SIZE - 1) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
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

        let shared_segments = process_state.shared_memfd_overlap_segments(start, end);
        let unmapped_len = if shared_segments.is_empty() {
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
        } else {
            for (segment_start, segment_len) in &shared_segments {
                let page_count = segment_len.div_ceil(PAGE_SIZE as usize);
                process_state
                    .address_space_mut()
                    .unmap_user_pages_without_free_at(VirtAddr::new(*segment_start), page_count)
                    .map_err(LinuxSysopError::AddressSpace)?;
            }
            process_state.release_shared_memfd_mappings_in_range(start, end);
            Some(user_len)
        };

        if let Some(unmapped_len) = unmapped_len {
            process_state
                .handles_mut()
                .clear_surface_mappings_in_range(start, unmapped_len);
        }
        if let Some(memory_map) = process_state.linux_memory_map_mut() {
            memory_map.unmap_range(start, end);
        }
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn mprotect(start: u64, user_len: u64, prot: u64) -> Result<(), LinuxSysopError> {
    validate_linux_protection(prot, true)?;
    if start & (PAGE_SIZE - 1) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(());
    }
    let page_flags = linux_mmap_page_flags(prot);

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let exec_path = String::from(process_state.exec_path());
        let end = start
            .checked_add(len as u64)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        let covering_vma = process_state
            .linux_memory_map()
            .and_then(|maps| maps.area_covering_range(start, end).cloned());
        if prot & linux_abi::PROT_EXEC != 0
            && !process_state
                .shared_memfd_overlap_segments(start, end)
                .is_empty()
        {
            return Err(LinuxSysopError::PermissionDenied);
        }

        {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            match address_space.protect_user_bytes(VirtAddr::new(start), len, page_flags) {
                Ok(()) => Ok(()),
                Err(paging::AddressSpaceError::NotMapped) => {
                    {
                        crate::debug::trace_loc!();
                        address_space.debug_dump_user_range_state(
                            VirtAddr::new(start),
                            len.div_ceil(PAGE_SIZE as usize),
                            "linux-mprotect-not-mapped",
                        );
                        crate::debug::println!(
                            "linux mprotect context: exec={} start={:#x} end={:#x} reserved={}",
                            exec_path,
                            start,
                            end,
                            linux_process_state
                                .as_ref()
                                .map(|state| state.is_range_reserved(start, end))
                                .unwrap_or(false),
                        );
                        if let Some(area) = covering_vma.as_ref() {
                            crate::debug::println!(
                                "linux mprotect context: vma=[{:#x},{:#x}) flags=R{}W{}X{}P{}",
                                area.start,
                                area.end,
                                if area.flags.read { "+" } else { "-" },
                                if area.flags.write { "+" } else { "-" },
                                if area.flags.execute { "+" } else { "-" },
                                if area.flags.private { "+" } else { "-" },
                            );
                        } else {
                            crate::debug::println!(
                                "linux mprotect context: no covering VMA exec={} start={:#x} end={:#x}",
                                exec_path,
                                start,
                                end,
                            );
                        }
                    }
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
            }?;
        }

        process_state
            .linux_memory_map_mut()
            .ok_or(LinuxSysopError::Unsupported)?
            .protect_range(start, end, linux_vma_flags_from_prot(prot, false))
            .map_err(|_| LinuxSysopError::AddressSpace(paging::AddressSpaceError::NotMapped))?;
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn brk(addr: u64) -> u64 {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return 0;
        }
        let (brk_start, brk_mapped_end) = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
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
            (state.brk_start, state.brk_mapped_end)
        };
        if let Some(memory_map) = process_state.linux_memory_map_mut() {
            memory_map.set_heap_range(brk_start, brk_mapped_end);
        }
        addr
    }) else {
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

fn validate_linux_protection(prot: u64, allow_exec: bool) -> Result<(), LinuxSysopError> {
    let supported_prot = linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC;
    if prot & !supported_prot != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if prot & linux_abi::PROT_WRITE != 0 && prot & linux_abi::PROT_EXEC != 0 {
        return Err(LinuxSysopError::PermissionDenied);
    }
    if !allow_exec && prot & linux_abi::PROT_EXEC != 0 {
        return Err(LinuxSysopError::PermissionDenied);
    }
    Ok(())
}

fn linux_vma_flags_from_prot(prot: u64, private: bool) -> linux_abi::LinuxVmaFlags {
    linux_abi::LinuxVmaFlags::new(
        prot & linux_abi::PROT_READ != 0,
        prot & linux_abi::PROT_WRITE != 0,
        prot & linux_abi::PROT_EXEC != 0,
        private,
    )
}

fn linux_vma_flags_from_mmap(prot: u64, flags: u64) -> linux_abi::LinuxVmaFlags {
    linux_vma_flags_from_prot(prot, !linux_mmap_is_shared(flags))
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
) -> Result<crate::memory::paging::UserRegion, LinuxSysopError> {
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
) -> Result<crate::memory::paging::UserRegion, LinuxSysopError> {
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
) -> Result<crate::memory::paging::UserRegion, LinuxSysopError> {
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
) -> Result<crate::memory::paging::UserRegion, LinuxSysopError> {
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
    Ok(crate::memory::paging::UserRegion {
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

    struct PreparedFileMapping {
        region: crate::memory::paging::UserRegion,
        file: crate::user::handles::VfsFileHandle,
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let file = match process_state.handles().get(fd) {
            Some(KernelHandle::VfsFile(file)) => file.clone(),
            Some(_) => return Ok(None),
            None => return Err(LinuxSysopError::BadFileDescriptor),
        };
        if offset & (PAGE_SIZE - 1) != 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        if flags & linux_abi::MAP_ANONYMOUS != 0 || !linux_mmap_is_private(flags) {
            return Err(LinuxSysopError::InvalidArgument);
        }
        if fixed_mapping && (requested_addr == 0 || requested_addr & (PAGE_SIZE - 1) != 0) {
            return Err(LinuxSysopError::InvalidArgument);
        }

        let page_flags = linux_mmap_page_flags(prot);
        let region = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            let Some(state) = linux_process_state.as_mut() else {
                crate::debug::println!("linux file mmap rejected: missing Linux process state");
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

        Ok(Some(PreparedFileMapping { region, file }))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    let Some(prepared) = result? else {
        return Ok(None);
    };

    let file_len = prepared.file.len();
    let file_path = prepared.file.path();
    let file_offset = usize::try_from(offset).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let Some(copy_result) = multitask::with_current_mm(|address_space| {
        if file_offset < file_len {
            let copy_len = file_map_len.min(file_len - file_offset);
            let mut copied = 0usize;
            let mut chunk = Vec::new();
            chunk.resize(copy_len.min(FILE_MMAP_COPY_CHUNK_LEN), 0);
            while copied < copy_len {
                let chunk_len = (copy_len - copied).min(chunk.len());
                let read = prepared
                    .file
                    .read_at(file_offset + copied, &mut chunk[..chunk_len]);
                if read == 0 {
                    break;
                }

                let chunk_ptr = prepared
                    .region
                    .start
                    .as_u64()
                    .checked_add(copied as u64)
                    .ok_or(LinuxSysopError::InvalidArgument)?;
                address_space
                    .initialize_user_bytes(VirtAddr::new(chunk_ptr), &chunk[..read])
                    .map_err(LinuxSysopError::AddressSpace)?;
                copied += read;
            }
        }

        address_space
            .ensure_user_region_mapped(prepared.region.start, prepared.region.page_count)
            .map_err(LinuxSysopError::AddressSpace)?;
        Ok(())
    }) else {
        rollback_failed_file_mapping(prepared.region);
        return Err(LinuxSysopError::Unsupported);
    };
    if let Err(err) = copy_result {
        rollback_failed_file_mapping(prepared.region);
        return Err(err);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let area = linux_abi::LinuxVma::new(
            prepared.region.start.as_u64(),
            prepared.region.end().as_u64(),
            offset,
            linux_vma_flags_from_mmap(prot, flags),
            linux_abi::LinuxVmaName::Path(file_path.clone()),
        )
        .ok_or(LinuxSysopError::InvalidArgument)?;
        let Some(memory_map) = process_state.linux_memory_map_mut() else {
            crate::debug::println!("linux file mmap rejected: missing Linux memory map");
            return Err(LinuxSysopError::Unsupported);
        };
        if fixed_mapping {
            memory_map.replace_area(area);
        } else {
            memory_map
                .insert_area(area)
                .map_err(|_| LinuxSysopError::NoMemory)?;
        }
        Ok(prepared.region.start.as_u64())
    }) else {
        rollback_failed_file_mapping(prepared.region);
        return Err(LinuxSysopError::Unsupported);
    };

    let mapped_addr = match result {
        Ok(mapped_addr) => mapped_addr,
        Err(err) => {
            rollback_failed_file_mapping(prepared.region);
            return Err(err);
        }
    };

    Ok(Some(mapped_addr))
}

fn rollback_failed_file_mapping(region: crate::memory::paging::UserRegion) {
    let _ = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return;
        }
        let _ = process_state
            .address_space_mut()
            .unmap_user_pages_at(region.start, region.page_count);
    });
}

fn mmap_current_process_memfd(
    fd: u64,
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    offset: u64,
) -> Result<Option<u64>, LinuxSysopError> {
    validate_linux_protection(prot, false)?;
    let file_map_len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let page_count = usize::try_from(user_len.div_ceil(PAGE_SIZE))
        .map_err(|_| LinuxSysopError::InvalidArgument)?;
    let fixed_mapping = flags & linux_abi::MAP_FIXED != 0;
    if fixed_mapping {
        return Err(LinuxSysopError::Unsupported);
    }
    if flags & linux_abi::MAP_ANONYMOUS != 0 || !linux_mmap_is_shared(flags) {
        return Ok(None);
    }
    if offset & (PAGE_SIZE - 1) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }
        let Some(handle) = process_state.handles().get(fd).cloned() else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };
        let KernelHandle::Memfd(memfd) = handle else {
            return Ok(None);
        };

        let offset = usize::try_from(offset).map_err(|_| LinuxSysopError::InvalidArgument)?;
        let writable = prot & linux_abi::PROT_WRITE != 0;
        let (frames, hold) = memfd.acquire_mapping(offset, file_map_len, writable)?;
        let page_flags = linux_mmap_page_flags(prot);
        let region = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            let Some(state) = linux_process_state.as_mut() else {
                crate::debug::println!("linux memfd mmap rejected: missing Linux process state");
                return Err(LinuxSysopError::Unsupported);
            };
            let region =
                reserve_linux_user_region(address_space, state, requested_addr, false, page_count)?;
            address_space
                .map_existing_user_pages_at(region.start, &frames, page_flags)
                .map_err(LinuxSysopError::AddressSpace)?
        };
        process_state.set_mapping_cursor(region.end().as_u64());
        process_state.record_shared_memfd_mapping(
            region.start.as_u64(),
            region.end().as_u64().saturating_sub(region.start.as_u64()),
            hold.clone(),
        );

        let area = linux_abi::LinuxVma::new(
            region.start.as_u64(),
            region.end().as_u64(),
            offset as u64,
            linux_vma_flags_from_mmap(prot, flags),
            linux_abi::LinuxVmaName::Path(hold.path()),
        )
        .ok_or(LinuxSysopError::InvalidArgument)?;
        let Some(memory_map) = process_state.linux_memory_map_mut() else {
            crate::debug::println!("linux memfd mmap rejected: missing Linux memory map");
            return Err(LinuxSysopError::Unsupported);
        };
        memory_map
            .insert_area(area)
            .map_err(|_| LinuxSysopError::NoMemory)?;

        Ok(Some(region.start.as_u64()))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

#[cfg(test)]
mod tests {
    use super::{mprotect, munmap, validate_linux_protection};
    use crate::user::linux as linux_abi;
    use crate::user::sysops::linux::LinuxSysopError;

    #[test]
    fn linux_mmap_protection_rejects_wx_and_disallowed_exec() {
        assert!(matches!(
            validate_linux_protection(linux_abi::PROT_WRITE | linux_abi::PROT_EXEC, true),
            Err(LinuxSysopError::PermissionDenied)
        ));
        assert!(matches!(
            validate_linux_protection(linux_abi::PROT_EXEC, false),
            Err(LinuxSysopError::PermissionDenied)
        ));
        assert!(
            validate_linux_protection(linux_abi::PROT_READ | linux_abi::PROT_WRITE, true).is_ok()
        );
    }

    #[test]
    fn munmap_rejects_unaligned_start() {
        assert!(matches!(
            munmap(1, 4096),
            Err(LinuxSysopError::InvalidArgument)
        ));
    }

    #[test]
    fn mprotect_rejects_unaligned_start() {
        assert!(matches!(
            mprotect(1, 4096, linux_abi::PROT_READ),
            Err(LinuxSysopError::InvalidArgument)
        ));
    }
}
