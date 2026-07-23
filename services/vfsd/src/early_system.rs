use rustos_user_abi::syscall::{
    EarlySystemBrokerArgs, EARLY_SYSTEM_BROKER_ABI_VERSION, EARLY_SYSTEM_BROKER_OP_INFO,
    EARLY_SYSTEM_BROKER_OP_READ, EARLY_SYSTEM_BROKER_PATH_CAPACITY, SYS_RUSTOS_EARLY_SYSTEM_BROKER,
};
use vfsd::bounded_early_system_chunk;

use super::{EIO, ENOENT, EOVERFLOW};

pub(super) fn file_len(path: &str) -> Result<Option<u64>, i32> {
    let mut out = 0_u64;
    let mut args = request(path, EARLY_SYSTEM_BROKER_OP_INFO)?;
    args.out_file_len_ptr = (&mut out as *mut u64) as u64;
    match syscall(&args) {
        Ok(0) if out != 0 => Ok(Some(out)),
        Ok(_) => Err(EIO),
        Err(ENOENT) => Ok(None),
        Err(errno) => Err(errno),
    }
}

pub(super) fn read(path: &str, offset: u64, out: &mut [u8]) -> Result<Option<usize>, i32> {
    let Some(file_len) = file_len(path)? else {
        return Ok(None);
    };
    if out.is_empty() {
        return Ok(Some(0));
    }
    let available = file_len.saturating_sub(offset);
    let target = out
        .len()
        .min(usize::try_from(available).unwrap_or(usize::MAX));
    let mut done = 0usize;
    while done < target {
        let chunk_len = bounded_early_system_chunk(target - done);
        let mut args = request(path, EARLY_SYSTEM_BROKER_OP_READ)?;
        args.offset = offset.checked_add(done as u64).ok_or(EOVERFLOW)?;
        args.buffer_ptr = out[done..].as_mut_ptr() as u64;
        args.buffer_len = chunk_len as u64;
        match syscall(&args) {
            Ok(read) if read <= chunk_len as u64 => {
                let read = read as usize;
                done += read;
                if read < chunk_len {
                    break;
                }
            }
            Ok(_) => return Err(EIO),
            // INFO established immutable ownership for this boot image. Losing
            // the same entry during READ is corruption, not volume fallback.
            Err(ENOENT) => return Err(EIO),
            Err(errno) => return Err(errno),
        }
    }
    Ok(Some(done))
}

fn request(path: &str, op: u16) -> Result<EarlySystemBrokerArgs, i32> {
    let path = path.strip_prefix('/').unwrap_or(path);
    if path.is_empty()
        || path.len() > EARLY_SYSTEM_BROKER_PATH_CAPACITY
        || path.as_bytes().contains(&0)
    {
        return Err(EOVERFLOW);
    }
    let mut args = EarlySystemBrokerArgs {
        abi_version: EARLY_SYSTEM_BROKER_ABI_VERSION,
        op,
        path_len: path.len() as u32,
        ..EarlySystemBrokerArgs::default()
    };
    args.path[..path.len()].copy_from_slice(path.as_bytes());
    Ok(args)
}

fn syscall(args: &EarlySystemBrokerArgs) -> Result<u64, i32> {
    let status = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_EARLY_SYSTEM_BROKER,
            (args as *const EarlySystemBrokerArgs) as u64,
        )
    };
    if status < 0 {
        Err((-status) as i32)
    } else {
        Ok(status as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_exact_relative_and_reserved_zero() {
        let args = request("/services/initd/initd.elf", EARLY_SYSTEM_BROKER_OP_INFO).unwrap();
        let expected = b"services/initd/initd.elf";
        assert_eq!(args.path_len as usize, expected.len());
        assert_eq!(&args.path[..expected.len()], expected);
        assert!(args.path[expected.len()..].iter().all(|byte| *byte == 0));
        assert_eq!(args.reserved0, 0);
        assert!(request("/", EARLY_SYSTEM_BROKER_OP_INFO).is_err());
    }
}
