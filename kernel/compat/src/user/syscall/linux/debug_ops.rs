use super::*;

pub(super) fn syscall_linux_rustos_debug_print(user_ptr: u64, user_len: u64) -> u64 {
    let requested_len = match usize::try_from(user_len) {
        Ok(len) => len,
        Err(_) => return linux_errno(LINUX_EINVAL),
    };
    if requested_len == 0 {
        return 0;
    }

    let len = requested_len.min(MAX_RUSTOS_DEBUG_PRINT_BYTES);
    let mut written = 0usize;
    let mut chunk = [0_u8; 256];
    while written < len {
        let chunk_len = (len - written).min(chunk.len());
        let ptr = match user_ptr.checked_add(written as u64) {
            Some(ptr) => ptr,
            None => return linux_errno(LINUX_EINVAL),
        };
        if let Err(err) = usermem::copy_from_current_user_exact(ptr, &mut chunk[..chunk_len]) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        debug::write_bytes(&chunk[..chunk_len]);
        written += chunk_len;
    }
    written as u64
}

pub(super) fn linux_errno(errno: i64) -> u64 {
    (-errno) as u64
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub(super) fn debug_user_path(path_ptr: u64) -> String {
    match usermem::read_current_user_c_string(path_ptr, 256) {
        Ok(path) => path,
        Err(_) => String::from("<invalid>"),
    }
}
