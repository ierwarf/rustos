//! Windows NT directory enumeration, named sections, pipes, and object queries.
//!
//! Replaces `wineserver` / `ntdll/unix/file.c` directory listing, named shared
//! memory sections, and pipe handles using RustOS `vfsd` + `local_fd` pipes.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::windows::{
    PAGE_SIZE, STATUS_BUFFER_OVERFLOW, STATUS_INFO_LENGTH_MISMATCH, STATUS_INVALID_HANDLE,
    STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER, STATUS_NO_MEMORY, STATUS_NO_MORE_FILES,
    STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, WIN32_EVENT_HANDLE_BASE,
    WIN32_FD_HANDLE_BASE, WIN32_KEY_HANDLE_BASE, WIN32_MUTANT_HANDLE_BASE,
    WIN32_OBJ_DIR_HANDLE_BASE, WIN32_PROCESS_HANDLE_BASE, WIN32_SECTION_HANDLE_BASE,
    WIN32_SEMAPHORE_HANDLE_BASE, WIN32_SYMLINK_HANDLE_BASE, WIN32_THREAD_HANDLE_BASE,
};

use super::super::SyscallFrame;
use super::super::linux::service_ops::{local_fd, vfs_meta, vfs_socket};
use super::nt_dispatch::{
    linux_errno_to_ntstatus, map_win32_file_handle, read_object_attributes_unicode, read_stack_arg,
    write_io_status, write_user_u32_checked, write_user_u64, UnicodeString64,
};
use crate::multitask;
use crate::user::process_state::{
    Win32DirEntrySnapshot, Win32DirEnumCursor, Win32SectionObject,
};
use crate::user::sysops::usermem;

const FILE_EPOCH_BIAS_100NS: i64 = 116_444_736_000_000_000;

#[derive(Clone)]
struct NamedSectionEntry {
    name: String,
    memfd: multitask::MemfdHandle,
    size: u64,
    protect: u64,
}

static WIN32_NAMED_SECTIONS: TrackedSpinLock<
    Vec<NamedSectionEntry>,
    { LockClass::CompatWaitset as u8 },
> = TrackedSpinLock::new(Vec::new());

fn normalize_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn install_memfd_handle(memfd: multitask::MemfdHandle) -> Result<u64, u64> {
    let entry = multitask::HandleEntry::new(
        multitask::KernelHandle::Memfd(memfd),
        0,
        linux_abi::O_RDWR,
    );
    match multitask::with_current_user_process_state_mut(|_, _, st| {
        st.handles_mut().install_entry(entry)
    }) {
        Some(Some(fd)) => Ok(fd),
        _ => Err(STATUS_NO_MEMORY),
    }
}

pub(super) fn register_or_open_named_section(
    raw_name: &str,
    size: u64,
    protect: u64,
    create: bool,
) -> Result<(u64, u64, u64), u64> {
    let key = normalize_name(raw_name);
    let mut reg = WIN32_NAMED_SECTIONS.lock();
    if let Some(existing) = reg.iter().find(|s| s.name == key) {
        let fd = install_memfd_handle(existing.memfd.reopen())?;
        return Ok((fd, existing.size, existing.protect));
    }
    if !create {
        return Err(STATUS_OBJECT_NAME_NOT_FOUND);
    }
    let charge_group = multitask::current_user_job_charge_group();
    let memfd = multitask::MemfdHandle::new(key.clone(), false, charge_group);
    let target_size = size.max(PAGE_SIZE);
    let _ = memfd.truncate(target_size as usize);
    let fd = install_memfd_handle(memfd.reopen())?;
    reg.push(NamedSectionEntry {
        name: key,
        memfd,
        size: target_size,
        protect,
    });
    Ok((fd, target_size, protect))
}

pub(super) fn nt_open_section(frame: &SyscallFrame) -> u64 {
    let (handle_ptr, _access, oa_ptr) = (frame.r10, frame.rdx, frame.r8);
    if handle_ptr == 0 || oa_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let Ok((_, raw_name)) = read_object_attributes_unicode(oa_ptr) else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let Ok((fd, size, protect)) = register_or_open_named_section(&raw_name, PAGE_SIZE, 4, false)
    else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_SECTION_HANDLE_BASE + objs.next_section_seq + 1;
        objs.next_section_seq += 1;
        objs.sections.push(Win32SectionObject {
            handle: h,
            fd: Some(fd),
            size,
            protect,
            is_image: false,
            name: Some(normalize_name(&raw_name)),
        });
        h
    });
    match handle {
        Some(h) => {
            if write_user_u64(handle_ptr, h) != STATUS_SUCCESS {
                let _ = vfs_socket::syscall_linux_vfs_close(fd);
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        None => {
            let _ = vfs_socket::syscall_linux_vfs_close(fd);
            STATUS_NO_MEMORY
        }
    }
}

pub(super) fn nt_create_named_pipe_file(frame: &SyscallFrame) -> u64 {
    let (handle_ptr, desired_access, oa_ptr, io_status_ptr) =
        (frame.r10, frame.rdx as u32, frame.r8, frame.r9);
    if handle_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let _ = oa_ptr;
    let nonblocking = (desired_access & 0x0010_0000) == 0;
    let Ok((read_fd, write_fd)) = local_fd::kernel_create_pipe_pair(nonblocking, false) else {
        return STATUS_NO_MEMORY;
    };
    // For Wine's anonymous CreatePipe / named pipe server bootstrap: expose read end by default,
    // or write end if caller requested strictly GENERIC_WRITE without GENERIC_READ.
    let wants_read = (desired_access & 0x8000_0001) != 0 || (desired_access & 0x4000_0002) == 0;
    let primary_fd = if wants_read { read_fd } else { write_fd };
    let extra_fd = if wants_read { write_fd } else { read_fd };
    let _ = vfs_socket::syscall_linux_vfs_close(extra_fd);

    let win_handle = WIN32_FD_HANDLE_BASE + primary_fd;
    if write_user_u64(handle_ptr, win_handle) != STATUS_SUCCESS {
        let _ = vfs_socket::syscall_linux_vfs_close(primary_fd);
        return STATUS_INVALID_PARAMETER;
    }
    write_io_status(io_status_ptr, STATUS_SUCCESS, 1);
    STATUS_SUCCESS
}

pub(super) fn nt_fs_control_file(frame: &SyscallFrame) -> u64 {
    let (handle, _event, _apc, _apc_ctx) = (frame.r10, frame.rdx, frame.r8, frame.r9);
    let io_status_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let fsctl_code = read_stack_arg(frame, 5).unwrap_or(0) as u32;
    let Some(_fd) = map_win32_file_handle(handle) else {
        return STATUS_INVALID_HANDLE;
    };
    // FSCTL_PIPE_LISTEN (0x110008), FSCTL_PIPE_DISCONNECT (0x110004), FSCTL_PIPE_WAIT (0x110018)
    match fsctl_code {
        0x0011_0008 | 0x0011_0004 | 0x0011_0018 => {
            write_io_status(io_status_ptr, STATUS_SUCCESS, 0);
            STATUS_SUCCESS
        }
        _ => rustos_user_abi::windows::STATUS_NOT_IMPLEMENTED,
    }
}

pub(super) fn wildcard_match_ci(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() || pattern == "*" || pattern == "*.*" {
        return true;
    }
    let p: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let t: Vec<u8> = text.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn parse_linux_dirent64_chunk(raw: &[u8]) -> Vec<Win32DirEntrySnapshot> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 19 <= raw.len() {
        let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
        if reclen < 19 || off + reclen > raw.len() {
            break;
        }
        let d_type = raw[off + 18];
        let name_slice = &raw[off + 19..off + reclen];
        let nul_pos = name_slice.iter().position(|&b| b == 0).unwrap_or(name_slice.len());
        if let Ok(name) = core::str::from_utf8(&name_slice[..nul_pos])
            && !name.is_empty()
        {
            out.push(Win32DirEntrySnapshot {
                name: String::from(name),
                d_type,
            });
        }
        off += reclen;
    }
    out
}

fn read_optional_unicode_pattern(us_ptr: u64) -> String {
    if us_ptr == 0 {
        return String::from("*");
    }
    let Ok(us) = usermem::read_current_user_struct::<UnicodeString64>(us_ptr) else {
        return String::from("*");
    };
    let len = usize::from(us.length);
    if len == 0 || us.buffer == 0 || (len & 1) != 0 || len > 1024 {
        return String::from("*");
    }
    let mut raw = vec![0_u8; len];
    if usermem::copy_from_current_user_exact(us.buffer, &mut raw).is_err() {
        return String::from("*");
    }
    let u16s: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16_lossy(&u16s);
    if s.is_empty() { String::from("*") } else { s }
}

pub(super) fn nt_query_directory_file(frame: &SyscallFrame) -> u64 {
    let (file_handle, _event, _apc, _apc_ctx) = (frame.r10, frame.rdx, frame.r8, frame.r9);
    let io_status_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let file_info_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    let length = read_stack_arg(frame, 6).unwrap_or(0) as usize;
    let info_class = (read_stack_arg(frame, 7).unwrap_or(0) as u32) as u64;
    let return_single = read_stack_arg(frame, 8).unwrap_or(0) != 0;
    let file_name_ptr = read_stack_arg(frame, 9).unwrap_or(0);
    let restart_scan = read_stack_arg(frame, 10).unwrap_or(0) != 0;

    if file_info_ptr == 0 || length < 16 {
        return STATUS_INVALID_PARAMETER;
    }
    let Some(fd) = map_win32_file_handle(file_handle) else {
        return STATUS_INVALID_HANDLE;
    };

    let (dir_path, cached_entries, mut cursor, exhausted) =
        multitask::with_current_user_process_state_mut(|_, _, st| {
            let objs = st.win32_objects();
            let path = objs
                .dir_paths
                .iter()
                .find(|(f, _)| *f == fd)
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| String::from("/"));
            if let Some(c) = objs.dir_cursors.iter().find(|c| c.fd == fd) {
                (path, Some((c.entries.clone(), c.pattern.clone())), c.cursor, c.exhausted)
            } else {
                (path, None, 0, false)
            }
        })
        .unwrap_or((String::from("/"), None, 0, false));

    let (entries, pattern) = if restart_scan || cached_entries.is_none() {
        let mut all_entries = Vec::new();
        let mut raw_buf = [0_u8; 4096];
        loop {
            match vfs_meta::kernel_vfs_getdents_raw(fd, &mut raw_buf) {
                Ok(0) => break,
                Ok(n) => all_entries.extend(parse_linux_dirent64_chunk(&raw_buf[..n])),
                Err(e) => return linux_errno_to_ntstatus(e),
            }
        }
        cursor = 0;
        (all_entries, read_optional_unicode_pattern(file_name_ptr))
    } else if exhausted {
        write_io_status(io_status_ptr, STATUS_NO_MORE_FILES, 0);
        return STATUS_NO_MORE_FILES;
    } else {
        let (e, p) = cached_entries.unwrap();
        let pat = if file_name_ptr != 0 {
            read_optional_unicode_pattern(file_name_ptr)
        } else {
            p
        };
        (e, pat)
    };

    let header_size: usize = match info_class {
        1 /* FileDirectoryInformation */ => 64,
        2 /* FileFullDirectoryInformation */ => 68,
        3 /* FileBothDirectoryInformation */ => 94,
        12 /* FileNamesInformation */ => 12,
        37 /* FileIdBothDirectoryInformation */ => 104,
        38 /* FileIdFullDirectoryInformation */ => 80,
        _ => return STATUS_INVALID_INFO_CLASS,
    };

    let mut out = Vec::new();
    let mut last_entry_start: Option<usize> = None;
    let start_cursor = cursor;

    while cursor < entries.len() {
        let ent = &entries[cursor];
        if !wildcard_match_ci(&pattern, &ent.name) {
            cursor += 1;
            continue;
        }
        let name_u16: Vec<u8> = ent
            .name
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let rec_len = (header_size + name_u16.len() + 7) & !7;
        if out.len() + header_size + name_u16.len() > length {
            if last_entry_start.is_none() {
                return STATUS_BUFFER_OVERFLOW;
            }
            break;
        }

        let mut is_dir = ent.d_type == 4;
        let mut file_size = 0_i64;
        let mut mtime = FILE_EPOCH_BIAS_100NS;
        let full_path = if dir_path == "/" {
            alloc::format!("/{}", ent.name)
        } else {
            alloc::format!("{}/{}", dir_path.trim_end_matches('/'), ent.name)
        };
        if let Ok(stx) = vfs_meta::kernel_vfs_statx_path(&full_path) {
            is_dir = (stx.stx_mode & 0o170_000) == 0o040_000;
            file_size = stx.stx_size as i64;
            mtime = (stx.stx_mtime.tv_sec.saturating_mul(10_000_000)) + FILE_EPOCH_BIAS_100NS;
        }
        let alloc_size = (file_size + 4095) & !4095;
        let attrs = if is_dir { 0x10_u32 } else { 0x80_u32 };

        let entry_start = out.len();
        if let Some(prev) = last_entry_start {
            let delta = (entry_start - prev) as u32;
            out[prev..prev + 4].copy_from_slice(&delta.to_le_bytes());
        }

        let mut rec = vec![0_u8; header_size + name_u16.len()];
        rec[4..8].copy_from_slice(&(cursor as u32).to_le_bytes());
        match info_class {
            12 /* FileNamesInformation */ => {
                rec[8..12].copy_from_slice(&(name_u16.len() as u32).to_le_bytes());
            }
            _ => {
                rec[8..16].copy_from_slice(&mtime.to_le_bytes());
                rec[16..24].copy_from_slice(&mtime.to_le_bytes());
                rec[24..32].copy_from_slice(&mtime.to_le_bytes());
                rec[32..40].copy_from_slice(&mtime.to_le_bytes());
                rec[40..48].copy_from_slice(&file_size.to_le_bytes());
                rec[48..56].copy_from_slice(&alloc_size.to_le_bytes());
                rec[56..60].copy_from_slice(&attrs.to_le_bytes());
                rec[60..64].copy_from_slice(&(name_u16.len() as u32).to_le_bytes());
            }
        }
        rec[header_size..].copy_from_slice(&name_u16);
        out.extend_from_slice(&rec);

        let pad = rec_len.saturating_sub(rec.len());
        if cursor + 1 < entries.len() && !return_single && out.len() + pad <= length {
            out.resize(out.len() + pad, 0);
        }

        last_entry_start = Some(entry_start);
        cursor += 1;
        if return_single {
            break;
        }
    }

    let now_exhausted = cursor >= entries.len();
    let _ = multitask::with_current_user_process_state_mut(|_, _, st| {
        let cursors = &mut st.win32_objects_mut().dir_cursors;
        cursors.retain(|c| c.fd != fd);
        cursors.push(Win32DirEnumCursor {
            fd,
            dir_path,
            pattern,
            entries,
            cursor,
            exhausted: now_exhausted,
        });
    });

    if out.is_empty() {
        let status = if start_cursor == 0 {
            STATUS_NO_SUCH_FILE
        } else {
            STATUS_NO_MORE_FILES
        };
        write_io_status(io_status_ptr, status, 0);
        return status;
    }

    if usermem::write_current_user_bytes(file_info_ptr, &out).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    write_io_status(io_status_ptr, STATUS_SUCCESS, out.len() as u64);
    STATUS_SUCCESS
}

pub(super) fn nt_query_object(frame: &SyscallFrame) -> u64 {
    let (handle, info_class, info_ptr, length) = (
        frame.r10,
        (frame.rdx as u32) as u64,
        frame.r8,
        (frame.r9 as u32) as usize,
    );
    let ret_len_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    match info_class {
        0 /* ObjectBasicInformation (56 bytes) */ => {
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, 56);
            }
            if length < 56 || info_ptr == 0 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let mut buf = [0_u8; 56];
            buf[4..8].copy_from_slice(&0x001f_0003_u32.to_le_bytes()); // GrantedAccess
            buf[8..12].copy_from_slice(&1_u32.to_le_bytes()); // HandleCount
            buf[12..16].copy_from_slice(&1_u32.to_le_bytes()); // PointerCount
            if usermem::write_current_user_bytes(info_ptr, &buf).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        2 /* ObjectTypeInformation */ => {
            let type_name = if (WIN32_EVENT_HANDLE_BASE..WIN32_MUTANT_HANDLE_BASE).contains(&handle)
            {
                "Event"
            } else if (WIN32_MUTANT_HANDLE_BASE..WIN32_SEMAPHORE_HANDLE_BASE).contains(&handle) {
                "Mutant"
            } else if (WIN32_SEMAPHORE_HANDLE_BASE..WIN32_THREAD_HANDLE_BASE).contains(&handle) {
                "Semaphore"
            } else if (WIN32_THREAD_HANDLE_BASE..WIN32_PROCESS_HANDLE_BASE).contains(&handle) {
                "Thread"
            } else if (WIN32_PROCESS_HANDLE_BASE..WIN32_KEY_HANDLE_BASE).contains(&handle) {
                "Process"
            } else if (WIN32_KEY_HANDLE_BASE..WIN32_OBJ_DIR_HANDLE_BASE).contains(&handle) {
                "Key"
            } else if (WIN32_OBJ_DIR_HANDLE_BASE..WIN32_SYMLINK_HANDLE_BASE).contains(&handle) {
                "Directory"
            } else if (WIN32_SYMLINK_HANDLE_BASE..).contains(&handle) {
                "SymbolicLink"
            } else if (WIN32_SECTION_HANDLE_BASE..WIN32_EVENT_HANDLE_BASE).contains(&handle) {
                "Section"
            } else {
                "File"
            };
            let u16_bytes: Vec<u8> = type_name
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect();
            let header = 104usize;
            let needed = header + u16_bytes.len() + 2;
            if ret_len_ptr != 0 {
                let _ = write_user_u32_checked(ret_len_ptr, needed as u32);
            }
            if length < needed || info_ptr == 0 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let str_va = info_ptr + header as u64;
            let mut buf = vec![0_u8; needed];
            buf[0..2].copy_from_slice(&(u16_bytes.len() as u16).to_le_bytes());
            buf[2..4].copy_from_slice(&((u16_bytes.len() + 2) as u16).to_le_bytes());
            buf[8..16].copy_from_slice(&str_va.to_le_bytes());
            buf[header..header + u16_bytes.len()].copy_from_slice(&u16_bytes);
            if usermem::write_current_user_bytes(info_ptr, &buf).is_err() {
                return STATUS_INVALID_PARAMETER;
            }
            STATUS_SUCCESS
        }
        _ => STATUS_INVALID_INFO_CLASS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching_is_case_insensitive() {
        assert!(wildcard_match_ci("*", "ntdll.dll"));
        assert!(wildcard_match_ci("*.*", "README.md"));
        assert!(wildcard_match_ci("*.EXE", "userdemo2.exe"));
        assert!(wildcard_match_ci("userdemo?.exe", "userdemo2.exe"));
        assert!(!wildcard_match_ci("*.dll", "userdemo2.exe"));
    }
}
