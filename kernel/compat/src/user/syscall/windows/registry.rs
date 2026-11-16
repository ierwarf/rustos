//! Windows NT Object Directory, Symbolic Links, and native Registry substrate.
//!
//! Replaces `wineserver`'s registry/object-namespace RPCs with a native Ring0
//! key-value tree and NT path directory seeded with Windows 10 / Wine defaults.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};
use rustos_user_abi::windows::{
    STATUS_BUFFER_TOO_SMALL, STATUS_INVALID_HANDLE, STATUS_INVALID_INFO_CLASS,
    STATUS_INVALID_PARAMETER, STATUS_NO_MEMORY, STATUS_NO_MORE_ENTRIES,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, WIN32_KEY_HANDLE_BASE, WIN32_OBJ_DIR_HANDLE_BASE,
    WIN32_SYMLINK_HANDLE_BASE,
};

use super::super::SyscallFrame;
use super::nt_dispatch::{
    read_object_attributes_unicode, read_stack_arg, write_user_u32_checked, write_user_u64,
    UnicodeString64,
};
use crate::multitask;
use crate::user::process_state::{
    Win32KeyHandleObject, Win32ObjDirHandleObject, Win32SymlinkHandleObject,
};
use crate::user::sysops::usermem;

const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_DWORD: u32 = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegValue {
    name: String,
    kind: u32,
    data: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct RegKey {
    canonical_path: String,
    values: BTreeMap<String, RegValue>,
}

struct RegistryStore {
    initialized: bool,
    keys: BTreeMap<String, RegKey>,
    symlinks: BTreeMap<String, (String, String)>,
}

impl RegistryStore {
    const fn new() -> Self {
        Self {
            initialized: false,
            keys: BTreeMap::new(),
            symlinks: BTreeMap::new(),
        }
    }

    fn ensure_seeded(&mut self) {
        if self.initialized {
            return;
        }
        self.initialized = true;

        self.insert_symlink("\\??\\C:", "\\Device\\HarddiskVolume1");
        self.insert_symlink("\\DosDevices\\C:", "\\??\\C:");
        self.insert_symlink("\\SystemRoot", "\\??\\C:\\windows");

        let ntv = "\\Registry\\Machine\\Software\\Microsoft\\Windows NT\\CurrentVersion";
        self.set_sz(ntv, "ProductName", "Windows 10 Pro");
        self.set_sz(ntv, "CurrentVersion", "6.3");
        self.set_sz(ntv, "CurrentBuild", "19045");
        self.set_sz(ntv, "CurrentBuildNumber", "19045");
        self.set_sz(ntv, "SystemRoot", "C:\\windows");
        self.set_sz(ntv, "PathName", "C:\\windows");
        self.set_dword(ntv, "CurrentMajorVersionNumber", 10);
        self.set_dword(ntv, "CurrentMinorVersionNumber", 0);

        let cp = "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Nls\\CodePage";
        self.set_sz(cp, "ACP", "1252");
        self.set_sz(cp, "OEMCP", "437");
        self.set_sz(cp, "MACCP", "10000");

        let lang = "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Nls\\Language";
        self.set_sz(lang, "Default", "0409");
        self.set_sz(lang, "InstallLanguage", "0409");

        let env =
            "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Session Manager\\Environment";
        self.set_expand_sz(env, "ComSpec", "C:\\windows\\system32\\cmd.exe");
        self.set_expand_sz(env, "SystemRoot", "C:\\windows");
        self.set_expand_sz(env, "windir", "C:\\windows");
        self.set_expand_sz(env, "OS", "Windows_NT");
        self.set_expand_sz(env, "Path", "C:\\windows\\system32;C:\\windows");

        let tz = "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\TimeZoneInformation";
        self.set_dword(tz, "Bias", 0);
        self.set_sz(tz, "StandardName", "UTC");
        self.set_sz(tz, "DaylightName", "UTC");

        self.ensure_key("\\Registry\\User\\.Default\\Software\\Wine");
        self.ensure_key("\\Registry\\Machine\\Software\\Wine");
    }

    fn insert_symlink(&mut self, path: &str, target: &str) {
        self.symlinks.insert(
            path.to_ascii_lowercase(),
            (String::from(path), String::from(target)),
        );
    }

    fn ensure_key(&mut self, path: &str) {
        let mut cur = String::new();
        for part in path.split('\\').filter(|s| !s.is_empty()) {
            cur.push('\\');
            cur.push_str(part);
            let norm = cur.to_ascii_lowercase();
            self.keys.entry(norm).or_insert_with(|| RegKey {
                canonical_path: cur.clone(),
                values: BTreeMap::new(),
            });
        }
    }

    fn set_sz(&mut self, path: &str, name: &str, value: &str) {
        self.set_utf16(path, name, REG_SZ, value);
    }

    fn set_expand_sz(&mut self, path: &str, name: &str, value: &str) {
        self.set_utf16(path, name, REG_EXPAND_SZ, value);
    }

    fn set_utf16(&mut self, path: &str, name: &str, kind: u32, value: &str) {
        let mut data = Vec::with_capacity((value.len() + 1) * 2);
        for u in value.encode_utf16() {
            data.extend_from_slice(&u.to_le_bytes());
        }
        data.extend_from_slice(&0_u16.to_le_bytes());
        self.set_raw(path, name, kind, data);
    }

    fn set_dword(&mut self, path: &str, name: &str, value: u32) {
        self.set_raw(path, name, REG_DWORD, value.to_le_bytes().to_vec());
    }

    fn set_raw(&mut self, path: &str, name: &str, kind: u32, data: Vec<u8>) {
        self.ensure_key(path);
        let norm = path.to_ascii_lowercase();
        if let Some(key) = self.keys.get_mut(&norm) {
            key.values.insert(
                name.to_ascii_lowercase(),
                RegValue {
                    name: String::from(name),
                    kind,
                    data,
                },
            );
        }
    }

    fn child_keys(&self, parent_norm: &str) -> Vec<&RegKey> {
        let prefix = if parent_norm == "\\" {
            String::from("\\")
        } else {
            alloc::format!("{parent_norm}\\")
        };
        self.keys
            .iter()
            .filter_map(|(k, v)| {
                let tail = k.strip_prefix(&prefix)?;
                if tail.is_empty() || tail.contains('\\') {
                    None
                } else {
                    Some(v)
                }
            })
            .collect()
    }
}

static WIN32_REGISTRY: TrackedSpinLock<RegistryStore, { LockClass::CompatWaitset as u8 }> =
    TrackedSpinLock::new(RegistryStore::new());

fn resolve_key_path(oa_ptr: u64) -> Result<String, u64> {
    let (root_handle, raw_name) = read_object_attributes_unicode(oa_ptr)?;
    let trimmed = raw_name.trim_matches('\\');
    if root_handle != 0 {
        let base = multitask::with_current_user_process_state_mut(|_, _, st| {
            st.win32_objects()
                .keys
                .iter()
                .find(|k| k.handle == root_handle)
                .map(|k| k.path.clone())
        })
        .flatten()
        .ok_or(STATUS_INVALID_HANDLE)?;
        if trimmed.is_empty() {
            return Ok(base);
        }
        return Ok(alloc::format!("{}\\{}", base.trim_end_matches('\\'), trimmed));
    }
    if trimmed.is_empty() {
        return Ok(String::from("\\Registry"));
    }
    let normalized = if raw_name.starts_with('\\') {
        alloc::format!("\\{trimmed}")
    } else {
        alloc::format!("\\Registry\\{trimmed}")
    };
    Ok(normalized)
}

fn read_unicode_string_ptr(us_ptr: u64) -> Result<String, u64> {
    if us_ptr == 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let us = usermem::read_current_user_struct::<UnicodeString64>(us_ptr)
        .map_err(|_| STATUS_INVALID_PARAMETER)?;
    let len = usize::from(us.length);
    if len == 0 {
        return Ok(String::new());
    }
    if us.buffer == 0 || (len & 1) != 0 || len > 4096 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let mut raw = vec![0_u8; len];
    usermem::copy_from_current_user_exact(us.buffer, &mut raw)
        .map_err(|_| STATUS_INVALID_PARAMETER)?;
    let u16s: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&u16s))
}

fn key_leaf_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn utf16_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

pub(super) fn nt_open_or_create_key(frame: &SyscallFrame, is_create: bool) -> u64 {
    let (handle_ptr, _access, oa_ptr) = (frame.r10, frame.rdx, frame.r8);
    if handle_ptr == 0 || oa_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let path = match resolve_key_path(oa_ptr) {
        Ok(p) => p,
        Err(status) => return status,
    };
    let norm = path.to_ascii_lowercase();
    let mut created = false;
    let canonical = {
        let mut reg = WIN32_REGISTRY.lock();
        reg.ensure_seeded();
        if let Some(existing) = reg.keys.get(&norm) {
            existing.canonical_path.clone()
        } else if is_create {
            reg.ensure_key(&path);
            created = true;
            reg.keys
                .get(&norm)
                .map(|k| k.canonical_path.clone())
                .unwrap_or(path)
        } else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        }
    };
    if is_create && let Ok(disp_ptr) = read_stack_arg(frame, 6)
        && disp_ptr != 0
    {
        let _ = write_user_u32_checked(disp_ptr, if created { 1 } else { 2 });
    }
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_KEY_HANDLE_BASE + objs.next_key_seq + 1;
        objs.next_key_seq += 1;
        objs.keys.push(Win32KeyHandleObject {
            handle: h,
            path: canonical,
        });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_set_value_key(frame: &SyscallFrame) -> u64 {
    let (key_handle, value_name_ptr, _title_idx, kind) =
        (frame.r10, frame.rdx, frame.r8, frame.r9 as u32);
    let data_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let data_len = read_stack_arg(frame, 5).unwrap_or(0) as usize;
    if data_len > 64 * 1024 || (data_len != 0 && data_ptr == 0) {
        return STATUS_INVALID_PARAMETER;
    }
    let Some(path) = lookup_key_path(key_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let name = match read_unicode_string_ptr(value_name_ptr) {
        Ok(n) => n,
        Err(status) => return status,
    };
    let mut data = vec![0_u8; data_len];
    if data_len != 0 && usermem::copy_from_current_user_exact(data_ptr, &mut data).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let mut reg = WIN32_REGISTRY.lock();
    reg.ensure_seeded();
    reg.set_raw(&path, &name, kind, data);
    STATUS_SUCCESS
}

pub(super) fn nt_query_value_key(frame: &SyscallFrame) -> u64 {
    let (key_handle, value_name_ptr, info_class, info_ptr) =
        (frame.r10, frame.rdx, (frame.r8 as u32) as u64, frame.r9);
    let length = read_stack_arg(frame, 4).unwrap_or(0) as usize;
    let res_len_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    let Some(path) = lookup_key_path(key_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let name = match read_unicode_string_ptr(value_name_ptr) {
        Ok(n) => n,
        Err(status) => return status,
    };
    let val = {
        let mut reg = WIN32_REGISTRY.lock();
        reg.ensure_seeded();
        let Some(k) = reg.keys.get(&path.to_ascii_lowercase()) else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        let Some(v) = k.values.get(&name.to_ascii_lowercase()) else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        v.clone()
    };
    write_value_info(info_class, info_ptr, length, res_len_ptr, &val)
}

pub(super) fn nt_enumerate_value_key(frame: &SyscallFrame) -> u64 {
    let (key_handle, index, info_class, info_ptr) = (
        frame.r10,
        (frame.rdx as u32) as usize,
        (frame.r8 as u32) as u64,
        frame.r9,
    );
    let length = read_stack_arg(frame, 4).unwrap_or(0) as usize;
    let res_len_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    let Some(path) = lookup_key_path(key_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let val = {
        let mut reg = WIN32_REGISTRY.lock();
        reg.ensure_seeded();
        let Some(k) = reg.keys.get(&path.to_ascii_lowercase()) else {
            return STATUS_INVALID_HANDLE;
        };
        let Some(v) = k.values.values().nth(index) else {
            return STATUS_NO_MORE_ENTRIES;
        };
        v.clone()
    };
    write_value_info(info_class, info_ptr, length, res_len_ptr, &val)
}

fn write_value_info(
    info_class: u64,
    info_ptr: u64,
    length: usize,
    res_len_ptr: u64,
    val: &RegValue,
) -> u64 {
    let name_bytes = utf16_bytes(&val.name);
    let payload = match info_class {
        0 /* KeyValueBasicInformation */ => {
            let needed = 12 + name_bytes.len();
            if res_len_ptr != 0 {
                let _ = write_user_u32_checked(res_len_ptr, needed as u32);
            }
            if length < needed {
                return STATUS_BUFFER_TOO_SMALL;
            }
            let mut buf = vec![0_u8; needed];
            buf[4..8].copy_from_slice(&val.kind.to_le_bytes());
            buf[8..12].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf[12..].copy_from_slice(&name_bytes);
            buf
        }
        1 /* KeyValueFullInformation */ => {
            let data_off = (20 + name_bytes.len() + 3) & !3;
            let needed = data_off + val.data.len();
            if res_len_ptr != 0 {
                let _ = write_user_u32_checked(res_len_ptr, needed as u32);
            }
            if length < needed {
                return STATUS_BUFFER_TOO_SMALL;
            }
            let mut buf = vec![0_u8; needed];
            buf[4..8].copy_from_slice(&val.kind.to_le_bytes());
            buf[8..12].copy_from_slice(&(data_off as u32).to_le_bytes());
            buf[12..16].copy_from_slice(&(val.data.len() as u32).to_le_bytes());
            buf[16..20].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf[20..20 + name_bytes.len()].copy_from_slice(&name_bytes);
            buf[data_off..].copy_from_slice(&val.data);
            buf
        }
        2 /* KeyValuePartialInformation */ => {
            let needed = 12 + val.data.len();
            if res_len_ptr != 0 {
                let _ = write_user_u32_checked(res_len_ptr, needed as u32);
            }
            if length < needed {
                return STATUS_BUFFER_TOO_SMALL;
            }
            let mut buf = vec![0_u8; needed];
            buf[4..8].copy_from_slice(&val.kind.to_le_bytes());
            buf[8..12].copy_from_slice(&(val.data.len() as u32).to_le_bytes());
            buf[12..].copy_from_slice(&val.data);
            buf
        }
        _ => return STATUS_INVALID_INFO_CLASS,
    };
    if info_ptr == 0 || usermem::write_current_user_bytes(info_ptr, &payload).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

pub(super) fn nt_enumerate_key(frame: &SyscallFrame) -> u64 {
    let (key_handle, index, info_class, info_ptr) = (
        frame.r10,
        (frame.rdx as u32) as usize,
        (frame.r8 as u32) as u64,
        frame.r9,
    );
    let length = read_stack_arg(frame, 4).unwrap_or(0) as usize;
    let res_len_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    let Some(path) = lookup_key_path(key_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let child_path = {
        let mut reg = WIN32_REGISTRY.lock();
        reg.ensure_seeded();
        let norm = path.to_ascii_lowercase();
        let children = reg.child_keys(&norm);
        let Some(child) = children.get(index) else {
            return STATUS_NO_MORE_ENTRIES;
        };
        child.canonical_path.clone()
    };
    write_key_info(info_class, info_ptr, length, res_len_ptr, &child_path, 0, 0)
}

pub(super) fn nt_query_key(frame: &SyscallFrame) -> u64 {
    let (key_handle, info_class, info_ptr, length) = (
        frame.r10,
        (frame.rdx as u32) as u64,
        frame.r8,
        (frame.r9 as u32) as usize,
    );
    let res_len_ptr = read_stack_arg(frame, 4).unwrap_or(0);
    let Some(path) = lookup_key_path(key_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let (canonical, subkeys, values) = {
        let mut reg = WIN32_REGISTRY.lock();
        reg.ensure_seeded();
        let norm = path.to_ascii_lowercase();
        let subkeys = reg.child_keys(&norm).len();
        let Some(k) = reg.keys.get(&norm) else {
            return STATUS_INVALID_HANDLE;
        };
        (k.canonical_path.clone(), subkeys, k.values.len())
    };
    write_key_info(info_class, info_ptr, length, res_len_ptr, &canonical, subkeys, values)
}

fn write_key_info(
    info_class: u64,
    info_ptr: u64,
    length: usize,
    res_len_ptr: u64,
    canonical_path: &str,
    subkeys: usize,
    values: usize,
) -> u64 {
    let leaf = utf16_bytes(key_leaf_name(canonical_path));
    let full = utf16_bytes(canonical_path);
    let payload = match info_class {
        0 /* KeyBasicInformation */ => {
            let needed = 16 + leaf.len();
            if res_len_ptr != 0 {
                let _ = write_user_u32_checked(res_len_ptr, needed as u32);
            }
            if length < needed {
                return STATUS_BUFFER_TOO_SMALL;
            }
            let mut buf = vec![0_u8; needed];
            buf[12..16].copy_from_slice(&(leaf.len() as u32).to_le_bytes());
            buf[16..].copy_from_slice(&leaf);
            buf
        }
        1 /* KeyNodeInformation */ => {
            let needed = 24 + leaf.len();
            if res_len_ptr != 0 {
                let _ = write_user_u32_checked(res_len_ptr, needed as u32);
            }
            if length < needed {
                return STATUS_BUFFER_TOO_SMALL;
            }
            let mut buf = vec![0_u8; needed];
            buf[12..16].copy_from_slice(&(needed as u32).to_le_bytes());
            buf[20..24].copy_from_slice(&(leaf.len() as u32).to_le_bytes());
            buf[24..].copy_from_slice(&leaf);
            buf
        }
        2 /* KeyFullInformation */ => {
            let needed = 44;
            if res_len_ptr != 0 {
                let _ = write_user_u32_checked(res_len_ptr, needed as u32);
            }
            if length < needed {
                return STATUS_BUFFER_TOO_SMALL;
            }
            let mut buf = vec![0_u8; needed];
            buf[12..16].copy_from_slice(&44_u32.to_le_bytes());
            buf[20..24].copy_from_slice(&(subkeys as u32).to_le_bytes());
            buf[28..32].copy_from_slice(&(values as u32).to_le_bytes());
            buf
        }
        3 /* KeyNameInformation */ => {
            let needed = 4 + full.len();
            if res_len_ptr != 0 {
                let _ = write_user_u32_checked(res_len_ptr, needed as u32);
            }
            if length < needed {
                return STATUS_BUFFER_TOO_SMALL;
            }
            let mut buf = vec![0_u8; needed];
            buf[0..4].copy_from_slice(&(full.len() as u32).to_le_bytes());
            buf[4..].copy_from_slice(&full);
            buf
        }
        _ => return STATUS_INVALID_INFO_CLASS,
    };
    if info_ptr == 0 || usermem::write_current_user_bytes(info_ptr, &payload).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

pub(super) fn nt_delete_key(key_handle: u64) -> u64 {
    let Some(path) = lookup_key_path(key_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let mut reg = WIN32_REGISTRY.lock();
    reg.ensure_seeded();
    if reg.keys.remove(&path.to_ascii_lowercase()).is_some() {
        STATUS_SUCCESS
    } else {
        STATUS_OBJECT_NAME_NOT_FOUND
    }
}

pub(super) fn nt_delete_value_key(key_handle: u64, value_name_ptr: u64) -> u64 {
    let Some(path) = lookup_key_path(key_handle) else {
        return STATUS_INVALID_HANDLE;
    };
    let name = match read_unicode_string_ptr(value_name_ptr) {
        Ok(n) => n,
        Err(status) => return status,
    };
    let mut reg = WIN32_REGISTRY.lock();
    reg.ensure_seeded();
    let Some(key) = reg.keys.get_mut(&path.to_ascii_lowercase()) else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    if key.values.remove(&name.to_ascii_lowercase()).is_some() {
        STATUS_SUCCESS
    } else {
        STATUS_OBJECT_NAME_NOT_FOUND
    }
}

fn lookup_key_path(handle: u64) -> Option<String> {
    multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .keys
            .iter()
            .find(|k| k.handle == handle)
            .map(|k| k.path.clone())
    })
    .flatten()
}

pub(super) fn nt_open_or_create_directory_object(frame: &SyscallFrame, _is_create: bool) -> u64 {
    let (handle_ptr, _access, oa_ptr) = (frame.r10, frame.rdx, frame.r8);
    if handle_ptr == 0 || oa_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let Ok((_, raw_name)) = read_object_attributes_unicode(oa_ptr) else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let norm = raw_name.trim_end_matches('\\');
    let path = if norm.is_empty() {
        String::from("\\")
    } else {
        String::from(norm)
    };
    let lower = path.to_ascii_lowercase();
    if !matches!(
        lower.as_str(),
        "\\" | "\\??" | "\\dosdevices" | "\\device" | "\\basenamedobjects" | "\\registry"
            | "\\registry\\machine" | "\\registry\\user"
    ) && !_is_create
    {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    }
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_OBJ_DIR_HANDLE_BASE + objs.next_obj_dir_seq + 1;
        objs.next_obj_dir_seq += 1;
        objs.obj_dirs.push(Win32ObjDirHandleObject { handle: h, path });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_query_directory_object(frame: &SyscallFrame) -> u64 {
    let (dir_handle, buf_ptr, length, single_entry) = (
        frame.r10,
        frame.rdx,
        (frame.r8 as u32) as usize,
        frame.r9 != 0,
    );
    let _restart = read_stack_arg(frame, 4).unwrap_or(0) != 0;
    let context_ptr = read_stack_arg(frame, 5).unwrap_or(0);
    let ret_len_ptr = read_stack_arg(frame, 6).unwrap_or(0);

    let dir_path = multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .obj_dirs
            .iter()
            .find(|d| d.handle == dir_handle)
            .map(|d| d.path.clone())
    })
    .flatten();
    let Some(dir_path) = dir_path else {
        return STATUS_INVALID_HANDLE;
    };
    let entries: &[(&str, &str)] = match dir_path.to_ascii_lowercase().as_str() {
        "\\" => &[
            ("??", "Directory"),
            ("Device", "Directory"),
            ("BaseNamedObjects", "Directory"),
            ("Registry", "Directory"),
            ("SystemRoot", "SymbolicLink"),
        ],
        "\\??" | "\\dosdevices" => &[("C:", "SymbolicLink"), ("Nsi", "Device")],
        "\\device" => &[("Afd", "Device"), ("HarddiskVolume1", "Device"), ("Nsi", "Device")],
        "\\registry" => &[("Machine", "Key"), ("User", "Key")],
        _ => &[],
    };
    let start_idx = if context_ptr != 0 {
        usermem::read_current_user_struct::<u32>(context_ptr).unwrap_or(0) as usize
    } else {
        0
    };
    let Some(&(name, kind)) = entries.get(start_idx) else {
        return STATUS_NO_MORE_ENTRIES;
    };
    let _ = single_entry;
    let name_u16 = utf16_bytes(name);
    let kind_u16 = utf16_bytes(kind);
    let header_sz = 32usize;
    let needed = header_sz + name_u16.len() + 2 + kind_u16.len() + 2;
    if ret_len_ptr != 0 {
        let _ = write_user_u32_checked(ret_len_ptr, needed as u32);
    }
    if buf_ptr == 0 || length < needed {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let name_va = buf_ptr + header_sz as u64;
    let kind_va = name_va + name_u16.len() as u64 + 2;
    let mut buf = vec![0_u8; needed];
    buf[0..2].copy_from_slice(&(name_u16.len() as u16).to_le_bytes());
    buf[2..4].copy_from_slice(&((name_u16.len() + 2) as u16).to_le_bytes());
    buf[8..16].copy_from_slice(&name_va.to_le_bytes());
    buf[16..18].copy_from_slice(&(kind_u16.len() as u16).to_le_bytes());
    buf[18..20].copy_from_slice(&((kind_u16.len() + 2) as u16).to_le_bytes());
    buf[24..32].copy_from_slice(&kind_va.to_le_bytes());
    buf[header_sz..header_sz + name_u16.len()].copy_from_slice(&name_u16);
    let kind_off = header_sz + name_u16.len() + 2;
    buf[kind_off..kind_off + kind_u16.len()].copy_from_slice(&kind_u16);
    if usermem::write_current_user_bytes(buf_ptr, &buf).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    if context_ptr != 0 {
        let _ = write_user_u32_checked(context_ptr, (start_idx + 1) as u32);
    }
    STATUS_SUCCESS
}

pub(super) fn nt_open_symbolic_link_object(frame: &SyscallFrame) -> u64 {
    let (handle_ptr, _access, oa_ptr) = (frame.r10, frame.rdx, frame.r8);
    if handle_ptr == 0 || oa_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let Ok((_, raw_name)) = read_object_attributes_unicode(oa_ptr) else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let lower = raw_name.to_ascii_lowercase();
    let entry = {
        let mut reg = WIN32_REGISTRY.lock();
        reg.ensure_seeded();
        reg.symlinks.get(&lower).cloned()
    };
    let Some((path, target)) = entry else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let handle = multitask::with_current_user_process_state_mut(|_, _, st| {
        let objs = st.win32_objects_mut();
        let h = WIN32_SYMLINK_HANDLE_BASE + objs.next_symlink_seq + 1;
        objs.next_symlink_seq += 1;
        objs.symlinks.push(Win32SymlinkHandleObject {
            handle: h,
            path,
            target,
        });
        h
    });
    match handle {
        Some(h) => write_user_u64(handle_ptr, h),
        None => STATUS_NO_MEMORY,
    }
}

pub(super) fn nt_query_symbolic_link_object(
    link_handle: u64,
    target_us_ptr: u64,
    ret_len_ptr: u64,
) -> u64 {
    if target_us_ptr == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let target = multitask::with_current_user_process_state_mut(|_, _, st| {
        st.win32_objects()
            .symlinks
            .iter()
            .find(|s| s.handle == link_handle)
            .map(|s| s.target.clone())
    })
    .flatten();
    let Some(target) = target else {
        return STATUS_INVALID_HANDLE;
    };
    let Ok(us) = usermem::read_current_user_struct::<UnicodeString64>(target_us_ptr) else {
        return STATUS_INVALID_PARAMETER;
    };
    let bytes = utf16_bytes(&target);
    if ret_len_ptr != 0 {
        let _ = write_user_u32_checked(ret_len_ptr, bytes.len() as u32);
    }
    if usize::from(us.maximum_length) < bytes.len() || us.buffer == 0 {
        return STATUS_BUFFER_TOO_SMALL;
    }
    if usermem::write_current_user_bytes(us.buffer, &bytes).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let mut us_raw = [0_u8; core::mem::size_of::<UnicodeString64>()];
    us_raw[0..2].copy_from_slice(&(bytes.len() as u16).to_le_bytes());
    us_raw[2..4].copy_from_slice(&us.maximum_length.to_le_bytes());
    us_raw[8..16].copy_from_slice(&us.buffer.to_le_bytes());
    if usermem::write_current_user_bytes(target_us_ptr, &us_raw).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    STATUS_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_seeds_defaults_and_supports_set_query_and_enum() {
        let mut store = RegistryStore::new();
        store.ensure_seeded();
        let ntv = "\\Registry\\Machine\\Software\\Microsoft\\Windows NT\\CurrentVersion"
            .to_ascii_lowercase();
        let key = store.keys.get(&ntv).expect("seeded CurrentVersion");
        assert!(key.values.contains_key("productname"));
        assert!(key.values.contains_key("systemroot"));

        store.set_dword("\\Registry\\Machine\\Software\\RustOS", "Version", 42);
        let custom = store
            .keys
            .get(&"\\Registry\\Machine\\Software\\RustOS".to_ascii_lowercase())
            .expect("custom key created");
        assert_eq!(custom.values["version"].data, 42_u32.to_le_bytes().to_vec());
        let children = store.child_keys("\\registry\\machine\\software");
        assert!(children.iter().any(|c| c.canonical_path.ends_with("RustOS")));
    }
}
