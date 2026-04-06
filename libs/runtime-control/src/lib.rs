use std::ffi::CString;
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const SYS_IOCTL: usize = 16;
const SYS_OPENAT: usize = 257;
const AT_FDCWD: isize = -100;
const O_RDWR: usize = 2;

const RUNTIME_IOCTL_SNAPSHOT_PROGRAMS: usize = 0x5254_0002;
const RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS: usize = 0x5254_0003;
const RUNTIME_IOCTL_REQUEST_LAUNCH: usize = 0x5254_0004;

const LAUNCH_TARGET_NEW_SESSION: u16 = 2;
const PROGRAM_NAME_CAPACITY: usize = 48;
const PROGRAM_PATH_CAPACITY: usize = 64;
const RUNNING_PROGRAM_NAME_CAPACITY: usize = 48;
const MAX_RUNTIME_PROGRAMS: usize = 64;

pub const DEFAULT_RUNTIME_DEVICE_PATH: &str = "/dev/runtime0";
pub const DEFAULT_STARTUP_REGISTRY_PATH: &str = "/system/registry/system/startup-programs.tsv";

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeProgram {
    pub program_id: u32,
    reserved: u32,
    weight_micros: u64,
    pub display_name: [u8; PROGRAM_NAME_CAPACITY],
    pub exec_path: [u8; PROGRAM_PATH_CAPACITY],
}

impl Default for RuntimeProgram {
    fn default() -> Self {
        Self {
            program_id: 0,
            reserved: 0,
            weight_micros: 0,
            display_name: [0; PROGRAM_NAME_CAPACITY],
            exec_path: [0; PROGRAM_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeRunningProgram {
    pub pid: u64,
    pub program_id: u32,
    reserved: u32,
    pub session_handle: u64,
    pub display_name: [u8; RUNNING_PROGRAM_NAME_CAPACITY],
}

impl Default for RuntimeRunningProgram {
    fn default() -> Self {
        Self {
            pid: 0,
            program_id: 0,
            reserved: 0,
            session_handle: 0,
            display_name: [0; RUNNING_PROGRAM_NAME_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeSnapshotProgramsRequest {
    programs_ptr: u64,
    capacity: u64,
    count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeSnapshotRunningProgramsRequest {
    programs_ptr: u64,
    capacity: u64,
    count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeLaunchRequest {
    program_id: u64,
    target_kind: u16,
    reserved: u16,
    reserved2: u32,
    target_value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StartupMode {
    Init,
    Session,
    Desktop,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StartupEntry {
    pub mode: StartupMode,
    pub display_name: String,
    pub exec: String,
    pub launch: String,
}

pub struct RuntimeClient {
    fd: OwnedFd,
}

impl RuntimeClient {
    pub fn open_default() -> Result<Self, i32> {
        Self::open(DEFAULT_RUNTIME_DEVICE_PATH)
    }

    pub fn open(path: &str) -> Result<Self, i32> {
        let path = CString::new(path).map_err(|_| libc::EINVAL)?;
        let raw_fd = openat(AT_FDCWD, &path, O_RDWR, 0)?;
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw_fd) },
        })
    }

    pub fn snapshot_programs(&self) -> Result<Vec<RuntimeProgram>, i32> {
        let mut programs = vec![RuntimeProgram::default(); MAX_RUNTIME_PROGRAMS];
        let mut request = RuntimeSnapshotProgramsRequest {
            programs_ptr: programs.as_mut_ptr() as u64,
            capacity: programs.len() as u64,
            count: 0,
        };
        ioctl_with_mut(
            self.fd.as_raw_fd(),
            RUNTIME_IOCTL_SNAPSHOT_PROGRAMS,
            &mut request,
        )?;
        let count = usize::try_from(request.count).unwrap_or(programs.len());
        programs.truncate(count.min(programs.len()));
        Ok(programs)
    }

    pub fn snapshot_running_programs(&self) -> Result<Vec<RuntimeRunningProgram>, i32> {
        let mut programs = vec![RuntimeRunningProgram::default(); MAX_RUNTIME_PROGRAMS];
        let mut request = RuntimeSnapshotRunningProgramsRequest {
            programs_ptr: programs.as_mut_ptr() as u64,
            capacity: programs.len() as u64,
            count: 0,
        };
        ioctl_with_mut(
            self.fd.as_raw_fd(),
            RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS,
            &mut request,
        )?;
        let count = usize::try_from(request.count).unwrap_or(programs.len());
        programs.truncate(count.min(programs.len()));
        Ok(programs)
    }

    pub fn request_launch_new_session(&self, program_id: u32) -> Result<(), i32> {
        let mut request = RuntimeLaunchRequest {
            program_id: program_id as u64,
            target_kind: LAUNCH_TARGET_NEW_SESSION,
            reserved: 0,
            reserved2: 0,
            target_value: 0,
        };
        ioctl_with_mut(
            self.fd.as_raw_fd(),
            RUNTIME_IOCTL_REQUEST_LAUNCH,
            &mut request,
        )
    }
}

pub fn decode_c_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn load_startup_entries(path: &str) -> Result<Vec<StartupEntry>, std::io::Error> {
    let contents = fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(entry) = parse_startup_entry(line) {
            entries.push(entry);
        }
    }
    entries.sort_by(|lhs, rhs| {
        lhs.mode
            .cmp(&rhs.mode)
            .then_with(|| lhs.exec.cmp(&rhs.exec))
            .then_with(|| lhs.display_name.cmp(&rhs.display_name))
    });
    Ok(entries)
}

fn parse_startup_entry(line: &str) -> Option<StartupEntry> {
    let mode = match field(line, "mode")? {
        "init" => StartupMode::Init,
        "session" => StartupMode::Session,
        "desktop" => StartupMode::Desktop,
        _ => return None,
    };
    Some(StartupEntry {
        mode,
        display_name: field(line, "display_name").unwrap_or("").to_string(),
        exec: field(line, "exec")?.to_string(),
        launch: field(line, "launch").unwrap_or("none").to_string(),
    })
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    for part in line.split('\t') {
        let (candidate_key, value) = part.split_once('=')?;
        if candidate_key == key {
            return Some(value);
        }
    }
    None
}

fn ioctl_with_mut<T>(fd: RawFd, request: usize, arg: &mut T) -> Result<(), i32> {
    let result = unsafe { syscall3(SYS_IOCTL, fd as usize, request, arg as *mut T as usize) };
    syscall_unit(result)
}

fn openat(dirfd: isize, path: &CString, flags: usize, mode: usize) -> Result<RawFd, i32> {
    let result = unsafe {
        syscall4(
            SYS_OPENAT,
            dirfd as usize,
            path.as_ptr() as usize,
            flags,
            mode,
        )
    };
    syscall_fd(result)
}

unsafe fn syscall3(number: usize, a0: usize, a1: usize, a2: usize) -> isize {
    libc::syscall(number as libc::c_long, a0, a1, a2) as isize
}

unsafe fn syscall4(number: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    libc::syscall(number as libc::c_long, a0, a1, a2, a3) as isize
}

fn syscall_unit(result: isize) -> Result<(), i32> {
    if result < 0 {
        Err((-result) as i32)
    } else {
        Ok(())
    }
}

fn syscall_fd(result: isize) -> Result<RawFd, i32> {
    if result < 0 {
        Err((-result) as i32)
    } else {
        Ok(result as RawFd)
    }
}
