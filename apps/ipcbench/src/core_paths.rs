//! Portable Linux ABI workloads: identical code runs in RustOS and a Linux guest.
//! Setup, semantic validation, cleanup, allocation and reporting are outside each
//! ordered-TSC interval, except when explicitly named (open+close, transfer+ACK).
//! Errors abort a complete probe; they are never accepted as fast samples.

use crate::{core_contract as contract, measure_stamped, report, skip, summarize, tsc};
use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
};

fn fd(value: i32) -> Option<OwnedFd> {
    (value >= 0).then(|| unsafe { OwnedFd::from_raw_fd(value) })
}

fn finish(name: &str, samples: Option<Vec<u64>>, khz: u64) {
    match samples {
        Some(mut samples) => report(name, &summarize(&mut samples), khz),
        None => skip(
            name,
            &format!("setup-or-semantics-failed-errno-{}", crate::last_errno()),
        ),
    }
}

fn sample(mut operation: impl FnMut() -> i64, valid: impl Fn(i64) -> bool) -> Option<u64> {
    let start = tsc();
    let status = operation();
    let end = tsc();
    valid(status).then_some(end.wrapping_sub(start))
}

pub fn identity(name: &str, khz: u64) {
    finish(name, identity_samples(), khz);
}

pub(crate) fn identity_samples() -> Option<Vec<u64>> {
    let expected = unsafe { crate::syscall0(crate::SYS_LINUX_GETPID) };
    (expected > 0)
        .then(|| {
            measure_stamped(contract::ITERS, contract::WARMUP, || {
                sample(
                    || unsafe { crate::syscall0(crate::SYS_LINUX_GETPID) },
                    |v| v == expected,
                )
            })
        })
        .flatten()
}

fn memfd() -> Option<OwnedFd> {
    fd(unsafe { libc::memfd_create(c"ipcbench-core".as_ptr(), libc::MFD_CLOEXEC) })
}

fn stream_pair() -> Option<(OwnedFd, OwnedFd)> {
    let mut pair = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return None;
    }
    Some((fd(pair[0])?, fd(pair[1])?))
}

fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut pair = [-1; 2];
    if unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    Some((fd(pair[0])?, fd(pair[1])?))
}

pub fn syscalls(khz: u64) {
    for &name in contract::SYSCALL_PROBES {
        finish(name, syscall_samples(name), khz);
    }
}

fn syscall_samples(name: &str) -> Option<Vec<u64>> {
    let iters = contract::ITERS;
    let warmup = contract::WARMUP;
    let path = crate::benchmark_executable();
    match name {
        "syscall_getuid" => {
            let expected = unsafe { crate::syscall0(crate::SYS_LINUX_GETUID) };
            if expected < 0 {
                return None;
            }
            measure_stamped(iters, warmup, || {
                sample(
                    || unsafe { crate::syscall0(crate::SYS_LINUX_GETUID) },
                    |v| v == expected,
                )
            })
        }
        "syscall_read_memfd" | "syscall_write_memfd" => {
            let file = memfd()?;
            let mut bytes = [0x5au8; 8];
            if unsafe { libc::write(file.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) } != 8 {
                return None;
            }
            measure_stamped(iters, warmup, || {
                if unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) } != 0 {
                    return None;
                }
                let mut output = [0u8; 8];
                if name == "syscall_write_memfd" {
                    bytes[0] = bytes[0].wrapping_add(1);
                }
                let start = tsc();
                let status = if name == "syscall_read_memfd" {
                    unsafe {
                        libc::read(file.as_raw_fd(), output.as_mut_ptr().cast(), output.len())
                    }
                } else {
                    unsafe { libc::write(file.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) }
                };
                let end = tsc();
                if status != 8 {
                    return None;
                }
                if name == "syscall_write_memfd" {
                    if unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) } != 0
                        || unsafe {
                            libc::read(file.as_raw_fd(), output.as_mut_ptr().cast(), output.len())
                        } != 8
                    {
                        return None;
                    }
                }
                (output == bytes).then_some(end.wrapping_sub(start))
            })
        }
        "syscall_openat" => measure_stamped(iters, warmup, || {
            let start = tsc();
            let result = unsafe {
                libc::openat(
                    libc::AT_FDCWD,
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC,
                )
            };
            let end = tsc();
            let file = fd(result)?;
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            (unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } == 0 && stat.st_size > 0)
                .then_some(end.wrapping_sub(start))
        }),
        "syscall_stat" => measure_stamped(iters, warmup, || {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let elapsed = sample(
                || unsafe { libc::fstatat(libc::AT_FDCWD, path.as_ptr(), &mut stat, 0) as i64 },
                |v| v == 0,
            )?;
            (stat.st_size > 0 && stat.st_mode & libc::S_IFMT == libc::S_IFREG).then_some(elapsed)
        }),
        "syscall_pipe2" => measure_stamped(iters, warmup, || {
            let mut pair = [-1; 2];
            let elapsed = sample(
                || unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC) as i64 },
                |v| v == 0,
            )?;
            let read = fd(pair[0])?;
            let write = fd(pair[1])?;
            if read.as_raw_fd() == write.as_raw_fd() {
                return None;
            }
            let token = [0x5au8];
            let mut output = [0u8];
            (unsafe { libc::write(write.as_raw_fd(), token.as_ptr().cast(), 1) } == 1
                && unsafe { libc::read(read.as_raw_fd(), output.as_mut_ptr().cast(), 1) } == 1
                && output == token)
                .then_some(elapsed)
        }),
        "syscall_epoll_wait_empty" => {
            let epoll = fd(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
            let mut event: libc::epoll_event = unsafe { std::mem::zeroed() };
            measure_stamped(iters, warmup, || {
                sample(
                    || unsafe { libc::epoll_wait(epoll.as_raw_fd(), &mut event, 1, 0) as i64 },
                    |v| v == 0,
                )
            })
        }
        "syscall_futex_wake_empty" => {
            let word = 0u32;
            measure_stamped(iters, warmup, || {
                sample(
                    || unsafe {
                        crate::syscall6(
                            libc::SYS_futex as u64,
                            &word as *const u32 as u64,
                            (libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG) as u64,
                            1,
                            0,
                            0,
                            0,
                        )
                    },
                    |v| v == 0,
                )
            })
        }
        _ => None,
    }
}

struct Mapping(*mut libc::c_void);
impl Mapping {
    fn new() -> Option<Self> {
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        (address != libc::MAP_FAILED).then_some(Self(address))
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                libc::munmap(self.0, 4096);
            }
        }
    }
}

pub fn memory(khz: u64) {
    for &name in contract::MEMORY_PROBES {
        if name == "cow_first_write" {
            finish(name, cow_samples(), khz);
            continue;
        }
        let samples = measure_stamped(contract::ITERS, contract::WARMUP, || {
            if name == "mmap_reserve_only" {
                let start = tsc();
                let mapping = Mapping::new();
                let end = tsc();
                // Mapping::new's tiny success wrapper is included; Drop is not.
                return mapping.map(|_| end.wrapping_sub(start));
            }
            let mut mapping = Mapping::new()?;
            if name == "page_fault_first_write" {
                let start = tsc();
                unsafe {
                    ptr::write_volatile(mapping.0.cast::<u8>(), 0x5a);
                }
                let end = tsc();
                return (unsafe { ptr::read_volatile(mapping.0.cast::<u8>()) } == 0x5a)
                    .then_some(end.wrapping_sub(start));
            }
            unsafe {
                ptr::write_volatile(mapping.0.cast::<u8>(), 0x5a);
            }
            if name == "munmap_resident_page" {
                let elapsed = sample(
                    || unsafe { libc::munmap(mapping.0, 4096) as i64 },
                    |v| v == 0,
                )?;
                mapping.0 = ptr::null_mut();
                Some(elapsed)
            } else {
                let elapsed = sample(
                    || unsafe { libc::mprotect(mapping.0, 4096, libc::PROT_READ) as i64 },
                    |v| v == 0,
                )?;
                (unsafe { ptr::read_volatile(mapping.0.cast::<u8>()) } == 0x5a).then_some(elapsed)
            }
        });
        finish(name, samples, khz);
    }
}

fn cow_samples() -> Option<Vec<u64>> {
    let stamp = unsafe { crate::LifecycleStampMapping::new() }.ok()?;
    measure_stamped(128, 8, || {
        let mapping = Mapping::new()?;
        unsafe {
            ptr::write_volatile(mapping.0.cast::<u8>(), 0x5a);
        }
        stamp.rewind().ok()?;
        let parent_begin = tsc();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let inherited = unsafe { ptr::read_volatile(mapping.0.cast::<u8>()) };
            let start = tsc();
            unsafe {
                ptr::write_volatile(mapping.0.cast::<u8>(), 0xa5);
            }
            let end = tsc();
            let valid = inherited == 0x5a
                && unsafe { ptr::read_volatile(mapping.0.cast::<u8>()) } == 0xa5
                && unsafe { crate::write_lifecycle_stamp(stamp.fd, start, end) }.is_ok();
            unsafe {
                libc::_exit(if valid { 0 } else { 125 });
            }
        }
        if pid < 0 {
            crate::debug_line(&format!(
                "ipcbench: cow-failure stage=fork errno={}",
                crate::last_errno()
            ));
            return None;
        }
        let mut status = 0;
        if let Err(errno) = unsafe { crate::wait_child(pid, &mut status) } {
            crate::debug_line(&format!("ipcbench: cow-failure stage=wait errno={errno}"));
            return None;
        }
        let parent_end = tsc();
        let (start, end) = match stamp.read() {
            Ok(stamps) => stamps,
            Err(errno) => {
                crate::debug_line(&format!(
                    "ipcbench: cow-failure stage=stamp-read errno={errno}"
                ));
                return None;
            }
        };
        if !crate::child_exited_successfully(status) {
            crate::debug_line(&format!(
                "ipcbench: cow-failure stage=child-status status={status}"
            ));
            return None;
        }
        if unsafe { ptr::read_volatile(mapping.0.cast::<u8>()) } != 0x5a {
            crate::debug_line("ipcbench: cow-failure stage=parent-private-value");
            return None;
        }
        if !crate::valid_lifecycle_window(parent_begin, parent_end, start, end) {
            crate::debug_line(&format!("ipcbench: cow-failure stage=stamp-window parent={parent_begin}..{parent_end} child={start}..{end}"));
            return None;
        }
        Some(end - start)
    })
}

fn write_all(file: i32, mut bytes: &[u8]) -> Result<(), i32> {
    while !bytes.is_empty() {
        let count = unsafe { libc::write(file, bytes.as_ptr().cast(), bytes.len()) };
        if count < 0 {
            let errno = crate::last_errno();
            if errno == libc::EINTR {
                continue;
            }
            return Err(errno);
        }
        if count == 0 {
            return Err(0);
        }
        if count as usize > bytes.len() {
            return Err(libc::EIO);
        }
        bytes = &bytes[count as usize..];
    }
    Ok(())
}
fn read_all(file: i32, mut bytes: &mut [u8]) -> Result<(), i32> {
    while !bytes.is_empty() {
        let count = unsafe { libc::read(file, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count < 0 {
            let errno = crate::last_errno();
            if errno == libc::EINTR {
                continue;
            }
            return Err(errno);
        }
        if count == 0 {
            return Err(0);
        } // EOF, not a stale thread-local errno.
        if count as usize > bytes.len() {
            return Err(libc::EIO);
        }
        bytes = &mut bytes[count as usize..];
    }
    Ok(())
}

fn ensure_fixture_directory() -> bool {
    if unsafe { libc::mkdir(c"/tmp".as_ptr(), 0o1777) } == 0 {
        return true;
    }
    if crate::last_errno() != libc::EEXIST {
        return false;
    }
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::stat(c"/tmp".as_ptr(), &mut stat) }) == 0
        && stat.st_mode & libc::S_IFMT == libc::S_IFDIR
}

struct SmallFiles(Vec<CString>);
impl Drop for SmallFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            unsafe {
                libc::unlink(path.as_ptr());
            }
        }
    }
}

fn small_files_samples() -> Option<Vec<u64>> {
    if !ensure_fixture_directory() {
        return None;
    }
    let mut files = SmallFiles(Vec::with_capacity(128));
    let bytes = [0x5au8; 4096];
    for index in 0..128 {
        let path = CString::new(format!("/tmp/ipcbench-small-{}-{index}", unsafe {
            libc::getpid()
        }))
        .ok()?;
        let file = fd(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        files.0.push(path);
        if write_all(file.as_raw_fd(), &bytes).is_err() {
            return None;
        }
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } != 0 || stat.st_size != 4096 {
            return None;
        }
    }
    measure_stamped(contract::DATA_ITERS, contract::DATA_WARMUP, || {
        sample(
            || {
                for path in &files.0 {
                    let file = unsafe {
                        libc::openat(
                            libc::AT_FDCWD,
                            path.as_ptr(),
                            libc::O_RDONLY | libc::O_CLOEXEC,
                        )
                    };
                    if file < 0 {
                        return -1;
                    }
                    if unsafe { libc::close(file) } != 0 {
                        return -1;
                    }
                }
                0
            },
            |value| value == 0,
        )
    })
}

struct Fixture {
    file: OwnedFd,
    path: CString,
}
impl Fixture {
    fn new() -> Option<Self> {
        if !ensure_fixture_directory() {
            return None;
        }
        let path =
            CString::new(format!("/tmp/ipcbench-core-{}", unsafe { libc::getpid() })).ok()?;
        let file = fd(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        })?;
        let fixture = Self { file, path };
        let bytes = vec![0x5au8; 1024 * 1024];
        write_all(fixture.file.as_raw_fd(), &bytes)
            .ok()
            .map(|()| fixture)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            libc::unlink(self.path.as_ptr());
        }
    }
}

fn data_size(name: &str) -> usize {
    if name.ends_with("_1b") {
        1
    } else if name.ends_with("_1m") {
        1024 * 1024
    } else if name.ends_with("_64k") {
        64 * 1024
    } else {
        4096
    }
}

pub fn data(khz: u64) {
    let fixture = Fixture::new();
    for &name in contract::DATA_PROBES {
        let samples = if name == "small_files_open_close_128" {
            small_files_samples()
        } else if name.starts_with("pipe_") || name.starts_with("socket_") {
            transfer(name.starts_with("socket_"), data_size(name))
        } else {
            fixture
                .as_ref()
                .and_then(|fixture| file_samples(name, fixture))
        };
        finish(name, samples, khz);
    }
}

fn file_samples(name: &str, fixture: &Fixture) -> Option<Vec<u64>> {
    let mut bytes = vec![0x5au8; data_size(name)];
    let mut sequence = 0usize;
    let mut output = vec![0u8; bytes.len()];
    measure_stamped(contract::DATA_ITERS, contract::DATA_WARMUP, || {
        if name == "file_open_close" {
            return sample(
                || unsafe {
                    let result = libc::openat(
                        libc::AT_FDCWD,
                        fixture.path.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC,
                    );
                    if result < 0 {
                        return -1;
                    }
                    libc::close(result) as i64
                },
                |v| v == 0,
            );
        }
        if unsafe { libc::lseek(fixture.file.as_raw_fd(), 0, libc::SEEK_SET) } != 0 {
            return None;
        }
        let reading = name.starts_with("file_read_");
        // Poison outside the interval: a successful no-copy read cannot pass.
        if reading {
            output.fill(0);
        } else {
            bytes.fill((sequence % 255 + 1) as u8);
            sequence += 1;
        }
        let start = tsc();
        let valid = if reading {
            read_all(fixture.file.as_raw_fd(), &mut output).is_ok()
        } else {
            write_all(fixture.file.as_raw_fd(), &bytes).is_ok()
        };
        let end = tsc();
        if !valid {
            return None;
        }
        if !reading {
            output.fill(0);
            if unsafe { libc::lseek(fixture.file.as_raw_fd(), 0, libc::SEEK_SET) } != 0
                || read_all(fixture.file.as_raw_fd(), &mut output).is_err()
            {
                return None;
            }
        }
        (output == bytes).then_some(end.wrapping_sub(start))
    })
}

// A persistent consumer validates every byte then ACKs. Reported time includes
// write/read loops, real wake/scheduling, receiver validation and the ACK; it is
// not a one-syscall sample, a disk measurement, or a spin-based handoff.
fn transfer(socket: bool, length: usize) -> Option<Vec<u64>> {
    let (receive, send) = if socket { stream_pair()? } else { pipe()? };
    // An independently available ACK channel lets the socket workload run even
    // when Linux pipe2 coverage is missing. Both guests pay for the same ACK.
    let (ack_read, ack_write) = stream_pair()?;
    let (iters, warmup) = if length == 1 {
        (contract::ITERS, contract::WARMUP)
    } else {
        (contract::DATA_ITERS, contract::DATA_WARMUP)
    };
    let mut bytes = vec![0u8; length];
    let mut output = vec![0u8; length];
    let worker = std::thread::Builder::new()
        .spawn(move || {
            for sequence in 0..iters + warmup {
                let expected = (sequence % 255 + 1) as u8;
                read_all(receive.as_raw_fd(), &mut output)
                    .map_err(|errno| ("consumer-read", errno))?;
                if output.iter().any(|&byte| byte != expected) {
                    return Err(("payload", 0));
                }
                write_all(ack_write.as_raw_fd(), &[expected])
                    .map_err(|errno| ("consumer-ack", errno))?;
                output.fill(0);
            }
            Ok(())
        })
        .ok()?;
    let mut sequence = 0usize;
    let samples = measure_stamped(iters, warmup, || {
        let expected = (sequence % 255 + 1) as u8;
        bytes.fill(expected);
        sequence += 1;
        let mut ack = [0u8];
        let start = tsc();
        let valid = write_all(send.as_raw_fd(), &bytes).is_ok()
            && read_all(ack_read.as_raw_fd(), &mut ack).is_ok();
        let end = tsc();
        (valid && ack == [expected]).then_some(end.wrapping_sub(start))
    });
    drop(send);
    drop(ack_read);
    match worker.join() {
        Ok(Ok(())) => samples,
        Ok(Err((stage, errno))) => {
            crate::debug_line(&format!("ipcbench: transfer-failure socket={socket} bytes={length} stage={stage} errno={errno}"));
            None
        }
        Err(_) => {
            crate::debug_line("ipcbench: transfer-worker-panicked");
            None
        }
    }
}

// Preload each message before timing so the receiver never relies on an
// unimplemented blocking read. This measures a buffered read, not transfer or
// wakeup latency; every returned byte is checked outside the timed interval.
fn buffered_socket_read_samples(length: usize) -> Option<Vec<u64>> {
    let (receive, send) = stream_pair()?;
    let mut input = vec![0u8; length];
    let mut output = vec![0u8; length];
    let mut sequence = 0usize;
    measure_stamped(contract::DATA_ITERS, contract::DATA_WARMUP, || {
        input.fill((sequence % 255 + 1) as u8);
        sequence += 1;
        output.fill(0);
        write_all(send.as_raw_fd(), &input).ok()?;
        let start = tsc();
        let result = read_all(receive.as_raw_fd(), &mut output);
        let end = tsc();
        (result.is_ok() && output == input).then_some(end.wrapping_sub(start))
    })
}

pub fn run(suite: &str, khz: u64) {
    match suite {
        contract::SYSCALL => syscalls(khz),
        contract::MEMORY => memory(khz),
        contract::DATA => data(khz),
        contract::SOCKET_BUFFERED => {
            for (&name, length) in contract::SOCKET_BUFFERED_PROBES.iter().zip([4096, 32768]) {
                finish(name, buffered_socket_read_samples(length), khz);
            }
        }
        _ => unreachable!("closed core suite vocabulary"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_socket_read_checks_full_payload_at_both_sizes() {
        for length in [4096, 32768] {
            assert_eq!(
                buffered_socket_read_samples(length).unwrap().len(),
                contract::DATA_ITERS
            );
        }
    }

    #[test]
    fn stream_helpers_preserve_errno_but_do_not_assign_stale_errno_to_eof() {
        let mut byte = [0u8];
        assert_eq!(read_all(-1, &mut byte), Err(libc::EBADF));
        assert_eq!(write_all(-1, &[1]), Err(libc::EBADF));
        let (read, write) = pipe().unwrap();
        write_all(write.as_raw_fd(), &[0x5a]).unwrap();
        read_all(read.as_raw_fd(), &mut byte).unwrap();
        assert_eq!(byte, [0x5a]);
        drop(write);
        unsafe {
            libc::close(-1);
        }
        assert_eq!(read_all(read.as_raw_fd(), &mut byte), Err(0));
    }

    #[test]
    fn syscall_validation_does_not_accept_errno_or_wrong_result() {
        assert!(sample(|| -libc::ENOSYS as i64, |v| v == 0).is_none());
        assert!(sample(|| 7, |v| v == 8).is_none());
        assert!(sample(|| 8, |v| v == 8).is_some());
    }

    #[test]
    fn child_stamps_must_belong_to_the_current_fork_iteration() {
        assert!(crate::valid_lifecycle_window(1000, 2000, 1000, 2000));
        assert!(crate::valid_lifecycle_window(1000, 2000, 1500, 1500));
        for (before, after) in [(0, 0), (100, 200), (999, 1500), (1500, 2001), (1600, 1500)] {
            assert!(!crate::valid_lifecycle_window(1000, 2000, before, after));
        }
    }

    #[test]
    fn size_and_suite_vocabulary_are_closed() {
        assert_eq!(data_size("file_read_4k"), 4096);
        assert_eq!(data_size("pipe_transfer_64k"), 65536);
        assert_eq!(data_size("socket_transfer_1m"), 1048576);
        assert!(!contract::is_suite("core_unknown"));
        for suite in [contract::SYSCALL, contract::MEMORY, contract::DATA] {
            let names = contract::probes(suite).unwrap();
            let unique: std::collections::BTreeSet<_> = names.iter().collect();
            assert_eq!(unique.len(), names.len());
        }
    }
}
