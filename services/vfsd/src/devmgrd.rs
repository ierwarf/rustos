use alloc::vec::Vec;
use core::mem::size_of;
use core::str;

use rustos_user_abi::syscall::{
    DEVMGRD_IPC_ABI_VERSION, DEVMGRD_IPC_OP_LOOKUP, DEVMGRD_IPC_OP_READDIR,
    DEVMGRD_MAX_DIR_ENTRIES, DEVMGRD_NODE_KIND_DEVICE, DEVMGRD_NODE_KIND_DIR,
    DevmgrdIpcRequest, DevmgrdIpcResponse, IPC_SERVICE_DEVMGRD, VFS_IPC_PATH_CAPACITY,
};
use rustos_svc_runtime::ipc;

use super::{DirEntry, RemoteKind};
use super::{EINVAL, ENODEV};

pub(super) fn devmgrd_lookup(path: &str) -> Result<RemoteKind, i32> {
    let mut request = devmgrd_request(DEVMGRD_IPC_OP_LOOKUP, path)?;
    let response = call_devmgrd(&mut request)?;
    devmgrd_kind_to_remote(response.kind)
}

pub(super) fn devmgrd_dir_entries(path: &str) -> Result<Vec<DirEntry>, i32> {
    let mut request = devmgrd_request(DEVMGRD_IPC_OP_READDIR, path)?;
    let response = call_devmgrd(&mut request)?;
    if response.entry_count as usize > DEVMGRD_MAX_DIR_ENTRIES {
        return Err(EINVAL);
    }
    let mut entries = Vec::new();
    for entry in response.entries.iter().take(response.entry_count as usize) {
        let name_len = entry.name_len as usize;
        if name_len == 0 || name_len > entry.name.len() {
            return Err(EINVAL);
        }
        let name = str::from_utf8(&entry.name[..name_len]).map_err(|_| EINVAL)?;
        entries.push(DirEntry::new(name, devmgrd_kind_to_remote(entry.kind)?));
    }
    Ok(entries)
}

fn devmgrd_request(op: u16, path: &str) -> Result<DevmgrdIpcRequest, i32> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(EINVAL);
    }
    let mut request = DevmgrdIpcRequest {
        op,
        path_len: bytes.len() as u32,
        ..DevmgrdIpcRequest::default()
    };
    request.path[..bytes.len()].copy_from_slice(bytes);
    Ok(request)
}

fn call_devmgrd(request: &mut DevmgrdIpcRequest) -> Result<DevmgrdIpcResponse, i32> {
    let endpoint = ipc::lookup_service_endpoint(IPC_SERVICE_DEVMGRD);
    if endpoint < 0 {
        return Err(ENODEV);
    }
    let mut response = DevmgrdIpcResponse::default();
    let received = unsafe {
        ipc::call(
            endpoint as u64,
            (request as *const DevmgrdIpcRequest).cast::<u8>(),
            size_of::<DevmgrdIpcRequest>(),
            (&mut response as *mut DevmgrdIpcResponse).cast::<u8>(),
            size_of::<DevmgrdIpcResponse>(),
        )
    };
    if received < 0 {
        return Err(ENODEV);
    }
    if received as usize != size_of::<DevmgrdIpcResponse>()
        || response.version != DEVMGRD_IPC_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i32);
    }
    Ok(response)
}

pub(super) fn devmgrd_kind_to_remote(kind: u16) -> Result<RemoteKind, i32> {
    match kind {
        DEVMGRD_NODE_KIND_DIR => Ok(RemoteKind::Directory),
        DEVMGRD_NODE_KIND_DEVICE => Ok(RemoteKind::Device),
        _ => Err(EINVAL),
    }
}
