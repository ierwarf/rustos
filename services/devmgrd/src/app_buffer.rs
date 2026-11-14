//! App DMA-BUF allocation policy. The kernel broker owns aperture custody and
//! DVM export; this module owns who may allocate what, bounded in size, and
//! drives the broker's timeout, cancellation and reaping. It never selects an
//! aperture offset and never trusts a client-supplied identity.
use std::mem::size_of;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use rustos_user_abi::syscall::{
    APP_BUFFER_BROKER_BEGIN, APP_BUFFER_BROKER_CANCEL, APP_BUFFER_BROKER_REAP,
    APP_BUFFER_BROKER_TAKE, APP_BUFFER_IPC_ALLOCATE, APP_BUFFER_IPC_VERSION,
    APP_BUFFER_MAX_BYTES, DevmgrdAppBufferRequest, DevmgrdAppBufferResponse,
    IpcReplyWithHandlesArgs, RustosAppBufferBrokerArgs, SYS_RUSTOS_APP_BUFFER_BROKER,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
};

const FORMAT_XRGB8888: u32 = 0x3432_5258;
const FORMAT_ARGB8888: u32 = 0x3432_5241;
const MAX_DIMENSION: u32 = 8192;
const BROKER_TIMEOUT_MS: u32 = 500;
/// The broker deadline plus time to drive its retirement before giving up.
const WORKER_BUDGET: Duration = Duration::from_millis(2500);
const IDLE_REAP: Duration = Duration::from_millis(250);
const QUEUE: usize = 4;

pub(crate) struct Work {
    pub(crate) reply_cap: u64,
    pub(crate) request: DevmgrdAppBufferRequest,
}

pub(crate) struct Worker {
    sender: SyncSender<Work>,
}

pub(crate) fn start() -> Result<Worker, i32> {
    let (sender, receiver) = mpsc::sync_channel::<Work>(QUEUE);
    thread::Builder::new()
        .name("devmgrd-appbuf".into())
        .spawn(move || run(receiver))
        .map_err(|err| err.raw_os_error().unwrap_or(libc::EAGAIN))?;
    Ok(Worker { sender })
}

impl Worker {
    /// Returns the reply capability when the bounded queue is saturated.
    pub(crate) fn try_send(&self, work: Work) -> Result<(), u64> {
        match self.sender.try_send(work) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(work) | TrySendError::Disconnected(work)) => Err(work.reply_cap),
        }
    }
}

/// Pure policy: every field is untrusted. The kernel stamped pid/tid; the
/// caller additionally proves they equal the IPC sender.
pub(crate) fn validate(request: &DevmgrdAppBufferRequest) -> Result<(), i32> {
    if request.version != APP_BUFFER_IPC_VERSION
        || request.op != APP_BUFFER_IPC_ALLOCATE
        || request.reserved != [0; 2]
        || request.pid == 0
    {
        return Err(libc::EINVAL);
    }
    if request.width == 0
        || request.height == 0
        || request.width > MAX_DIMENSION
        || request.height > MAX_DIMENSION
        || !matches!(request.format, FORMAT_XRGB8888 | FORMAT_ARGB8888)
        || request.stride % 4 != 0
        || u64::from(request.stride) < u64::from(request.width) * 4
    {
        return Err(libc::EINVAL);
    }
    let bytes = u64::from(request.stride) * u64::from(request.height);
    if bytes > APP_BUFFER_MAX_BYTES {
        return Err(libc::EFBIG);
    }
    Ok(())
}

/// Returns a non-negative result or `-errno`.
fn broker(args: &mut RustosAppBufferBrokerArgs) -> i64 {
    // SAFETY: `args` is a live, correctly laid out RustOS ABI structure.
    let result = unsafe {
        libc::syscall(SYS_RUSTOS_APP_BUFFER_BROKER as libc::c_long, args as *mut RustosAppBufferBrokerArgs as u64)
    } as i64;
    if result == -1 {
        let errno = std::io::Error::last_os_error().raw_os_error().filter(|e| *e > 0).unwrap_or(libc::EIO);
        -i64::from(errno)
    } else {
        result
    }
}

fn allocate(request: &DevmgrdAppBufferRequest) -> Result<u64, i32> {
    let mut begin = RustosAppBufferBrokerArgs {
        op: APP_BUFFER_BROKER_BEGIN,
        timeout_ms: BROKER_TIMEOUT_MS,
        width: request.width,
        height: request.height,
        stride: request.stride,
        format: request.format,
        sequence: 0,
    };
    let sequence = broker(&mut begin);
    if sequence < 0 {
        return Err((-sequence) as i32);
    }
    let started = Instant::now();
    loop {
        let mut take = RustosAppBufferBrokerArgs {
            op: APP_BUFFER_BROKER_TAKE,
            sequence: sequence as u64,
            ..RustosAppBufferBrokerArgs::default()
        };
        let result = broker(&mut take);
        if result >= 0 {
            return Ok(result as u64);
        }
        let errno = (-result) as i32;
        if errno == libc::EAGAIN && started.elapsed() < WORKER_BUDGET {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        if errno == libc::EAGAIN {
            let mut cancel = RustosAppBufferBrokerArgs {
                op: APP_BUFFER_BROKER_CANCEL,
                sequence: sequence as u64,
                ..RustosAppBufferBrokerArgs::default()
            };
            let _ = broker(&mut cancel);
        }
        // ECANCELED: the broker finished retiring a timed-out allocation.
        return Err(match errno {
            libc::ECANCELED | libc::EAGAIN | libc::ETIMEDOUT => libc::ETIMEDOUT,
            other => other,
        });
    }
}

fn reap() -> i64 {
    let mut args = RustosAppBufferBrokerArgs { op: APP_BUFFER_BROKER_REAP, ..Default::default() };
    broker(&mut args)
}

/// Drives retirement and scrubbing until quiescent, bounded by `budget`.
fn drain(budget: Duration) {
    let started = Instant::now();
    while reap() == -(libc::EAGAIN as i64) && started.elapsed() < budget {
        thread::sleep(Duration::from_millis(2));
    }
}

fn reply(work: &Work, status: i32, fd: Option<u64>) {
    let response = DevmgrdAppBufferResponse { version: APP_BUFFER_IPC_VERSION, status };
    match fd {
        None => {
            crate::syscall3(
                SYS_RUSTOS_IPC_REPLY,
                work.reply_cap,
                (&response as *const DevmgrdAppBufferResponse) as u64,
                size_of::<DevmgrdAppBufferResponse>() as u64,
            );
        }
        Some(fd) => {
            let args = IpcReplyWithHandlesArgs {
                reply_cap: work.reply_cap,
                response_ptr: (&response as *const DevmgrdAppBufferResponse) as u64,
                response_len: size_of::<DevmgrdAppBufferResponse>() as u64,
                send_fds_ptr: (&fd as *const u64) as u64,
                send_fd_count: 1,
                reserved0: 0,
                reserved1: 0,
            };
            crate::syscall1(
                SYS_RUSTOS_IPC_REPLY_WITH_HANDLES,
                (&args as *const IpcReplyWithHandlesArgs) as u64,
            );
            // SAFETY: the sender-side fd is still owned here; the transfer
            // duplicated authority. Closing it drops devmgrd's reference so the
            // buffer becomes reclaimable once the client and mappings are gone.
            unsafe { libc::close(fd as libc::c_int) };
        }
    }
}

fn run(receiver: Receiver<Work>) {
    loop {
        match receiver.recv_timeout(IDLE_REAP) {
            Ok(work) => {
                if let Err(errno) = validate(&work.request) {
                    reply(&work, errno, None);
                    continue;
                }
                // Settle earlier retirements first so exhaustion reflects reuse.
                drain(Duration::from_millis(50));
                match allocate(&work.request) {
                    Ok(fd) => reply(&work, 0, Some(fd)),
                    Err(errno) => {
                        reply(&work, errno, None);
                        drain(WORKER_BUDGET);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => drain(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ok() -> DevmgrdAppBufferRequest {
        DevmgrdAppBufferRequest {
            version: APP_BUFFER_IPC_VERSION,
            op: APP_BUFFER_IPC_ALLOCATE,
            pid: 7,
            tid: 8,
            width: 64,
            height: 32,
            stride: 256,
            format: FORMAT_XRGB8888,
            reserved: [0; 2],
        }
    }
    #[test]
    fn well_formed_linear_requests_are_admitted() {
        assert_eq!(validate(&ok()), Ok(()));
        let argb = DevmgrdAppBufferRequest { format: FORMAT_ARGB8888, stride: 260, ..ok() };
        assert_eq!(validate(&argb), Ok(()));
    }
    #[test]
    fn malformed_unsupported_or_oversized_requests_are_refused() {
        let r = ok();
        for bad in [
            DevmgrdAppBufferRequest { version: 2, ..r },
            DevmgrdAppBufferRequest { op: 9, ..r },
            DevmgrdAppBufferRequest { pid: 0, ..r },
            DevmgrdAppBufferRequest { reserved: [1, 0], ..r },
            DevmgrdAppBufferRequest { width: 0, ..r },
            DevmgrdAppBufferRequest { height: MAX_DIMENSION + 1, ..r },
            DevmgrdAppBufferRequest { stride: 255, ..r },
            DevmgrdAppBufferRequest { stride: 252, ..r },
            DevmgrdAppBufferRequest { format: 0x3231_564e, ..r },
        ] {
            assert_eq!(validate(&bad), Err(libc::EINVAL), "{bad:?}");
        }
        let huge = DevmgrdAppBufferRequest { width: 8192, height: 8192, stride: 32768, ..r };
        assert_eq!(validate(&huge), Err(libc::EFBIG));
    }
    #[test]
    fn request_size_cannot_alias_other_devmgrd_envelopes() {
        use rustos_user_abi::syscall::*;
        let size = size_of::<DevmgrdAppBufferRequest>();
        assert_ne!(size, size_of::<DevmgrdIpcRequest>());
        assert_ne!(size, size_of::<DevmgrdDeviceOpenRequest>());
        assert_ne!(size, size_of::<DevmgrdDeviceIoctlRequest>());
        assert_ne!(size, size_of::<CommercialMaxProtocolRequest>());
    }
}
