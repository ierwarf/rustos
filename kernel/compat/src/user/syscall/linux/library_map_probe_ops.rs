//! Read-only runtime proof hooks for shared library mappings.
//!
//! - **Owner:** compat; evidence only, never authority or policy.
//! - **Boundary:** only a process whose exec path is exactly
//!   `LIBRARY_MAP_PROBE_EXEC_PATH` may ask; it sees its own page tables only.
//! - **Lifecycle:** stateless; every call reads under the retained process.
//! - **Failure:** any other caller gets `EPERM`, a malformed request `EINVAL`.
//! - **Forbidden:** mutation, other processes' frames, any decision input.
use super::*;
use rustos_user_abi::syscall::{
    LibraryMapProbeFrame, LibraryMapProbeStats, RustosLibraryMapProbeArgs,
    LIBRARY_MAP_PROBE_EXEC_PATH, LIBRARY_MAP_PROBE_FRAME_PRESENT, LIBRARY_MAP_PROBE_FRAME_WRITABLE,
    LIBRARY_MAP_PROBE_MAX_PAGES, LIBRARY_MAP_PROBE_OP_FRAMES, LIBRARY_MAP_PROBE_OP_STATS,
};

const PAGE: u64 = 4096;

pub(super) fn control(args_ptr: u64) -> u64 {
    linux_unit_result((|| {
        let args = read_user_struct::<RustosLibraryMapProbeArgs>(args_ptr)?;
        match args.op {
            LIBRARY_MAP_PROBE_OP_FRAMES => frames(&args),
            LIBRARY_MAP_PROBE_OP_STATS => stats(&args),
            _ => Err(LINUX_EINVAL),
        }
    })())
}

fn frames(args: &RustosLibraryMapProbeArgs) -> LinuxResult<()> {
    if args.count == 0
        || args.count > LIBRARY_MAP_PROBE_MAX_PAGES
        || args.addr % PAGE != 0
        || args.out_ptr == 0
    {
        return Err(LINUX_EINVAL);
    }
    let mut out = [LibraryMapProbeFrame::default(); LIBRARY_MAP_PROBE_MAX_PAGES as usize];
    let mut read = |state: &kernel_ps::api::process_state::UserProcessState| {
        if state.exec_path() != LIBRARY_MAP_PROBE_EXEC_PATH {
            return false;
        }
        for index in 0..args.count as usize {
            let Some(addr) = args.addr.checked_add(index as u64 * PAGE) else {
                break;
            };
            if let Some((phys, flags)) = state
                .address_space()
                .translate_user_with_flags(x86_64::VirtAddr::new(addr))
            {
                let mut bits = LIBRARY_MAP_PROBE_FRAME_PRESENT;
                if flags.contains(crate::memory::PageTableFlags::WRITABLE) {
                    bits |= LIBRARY_MAP_PROBE_FRAME_WRITABLE;
                }
                out[index] = LibraryMapProbeFrame { phys: phys.as_u64(), flags: bits };
            }
        }
        true
    };
    // The caller itself must be the probe executable, whatever it inspects.
    let caller_ok = multitask::with_current_user_process_state(|_, _, state| {
        state.exec_path() == LIBRARY_MAP_PROBE_EXEC_PATH
    })
    .ok_or(LINUX_ESRCH)?;
    if !caller_ok {
        return Err(LINUX_EPERM);
    }
    let permitted = if args.pid == 0 {
        multitask::with_current_user_process_state(|_, _, state| read(state))
    } else {
        multitask::with_process_state_by_pid(args.pid, |state| read(state))
    }
    .ok_or(LINUX_ESRCH)?;
    if !permitted {
        return Err(LINUX_EPERM);
    }
    for (index, frame) in out.iter().take(args.count as usize).enumerate() {
        write_user_struct(args.out_ptr + index as u64 * 16, frame)?;
    }
    Ok(())
}

fn stats(args: &RustosLibraryMapProbeArgs) -> LinuxResult<()> {
    if args.out_ptr == 0 {
        return Err(LINUX_EINVAL);
    }
    let own_data_leaves = multitask::with_current_user_process_state(|_, _, state| {
        (state.exec_path() == LIBRARY_MAP_PROBE_EXEC_PATH).then(|| {
            crate::memory::phys::data_leaf_count(state.address_space().root_phys().as_u64())
                .unwrap_or(0)
        })
    })
    .ok_or(LINUX_ESRCH)?
    .ok_or(LINUX_EPERM)?;
    let (sealed_objects, reserved_pages) = crate::library_cache::REGISTRY.lock().census();
    let (alias_maps, alias_pages) = crate::library_cache::alias_counters();
    let (decline_counts, decline_details) = crate::library_cache::decline_counters();
    let stats = LibraryMapProbeStats {
        free_pages: crate::memory::phys::free_bytes() / PAGE,
        own_data_leaves,
        sealed_objects,
        reserved_pages,
        alias_maps,
        alias_pages,
        decline_counts,
        decline_details,
    };
    write_user_struct(args.out_ptr, &stats)
}
