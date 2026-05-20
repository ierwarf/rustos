use std::collections::VecDeque;
use std::io::Write;
use std::mem::size_of;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    InputIngestBrokerArgs, InputIngressWire, InputStatsBrokerArgs, InputStatsWire,
    InputdIpcRequest, InputdIpcResponse, InputdReadResponse, INPUTD_ACCESS_EVDEV,
    INPUTD_ACCESS_NATIVE, INPUTD_INGEST_MAX_EVENTS, INPUTD_IPC_ABI_VERSION,
    INPUTD_IPC_OP_AUTHORIZE_READ, INPUTD_IPC_OP_DRAIN_INGEST, INPUTD_IPC_OP_PING,
    INPUTD_IPC_OP_READ, INPUTD_IPC_OP_STATS, INPUTD_READ_FLAG_NONBLOCK,
    INPUTD_READ_PAYLOAD_CAPACITY, IPC_SERVICE_INPUTD, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_INPUT_INGEST_BROKER, SYS_RUSTOS_INPUT_STATS_BROKER, SYS_RUSTOS_IPC_ENDPOINT_CREATE,
    SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY,
};

const RECV_BACKOFF: Duration = Duration::from_millis(50);
const INPUTD_QUEUE_MAX_EVENTS: usize = 4096;
const INPUTD_MAX_NATIVE_READ_BYTES: u64 = input_evdev::MAX_NATIVE_READ_BYTES as u64;
const INPUTD_MAX_EVDEV_READ_BYTES: u64 = input_evdev::MAX_EVDEV_READ_BYTES as u64;
static INPUT_DELIVERY_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct InputQueue {
    events: VecDeque<input_evdev::InputEvent>,
    dropped_lossy: u64,
}

impl InputQueue {
    fn len(&self) -> usize {
        self.events.len()
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn push(&mut self, event: input_evdev::InputEvent) {
        if self.events.len() >= INPUTD_QUEUE_MAX_EVENTS {
            let _ = self.events.pop_front();
            self.dropped_lossy = self.dropped_lossy.saturating_add(1);
        }
        self.events.push_back(event);
    }

    fn pop_front(&mut self) -> Option<input_evdev::InputEvent> {
        self.events.pop_front()
    }
}

fn main() {
    observability_client::info!("inputd", service, "service started");
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }
    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_INPUTD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "inputd: endpoint register failed errno={}",
            -register
        );
        return;
    }
    debug_line("inputd: input policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    let mut queue = InputQueue::default();
    loop {
        let mut request = InputdIpcRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut InputdIpcRequest) as u64,
            size_of::<InputdIpcRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }
        let reply = if request.op == INPUTD_IPC_OP_READ {
            let mut response = InputdReadResponse {
                version: INPUTD_IPC_ABI_VERSION,
                op: request.op,
                ..InputdReadResponse::default()
            };
            response.status = match validate(received as usize, &request) {
                Ok(()) => dispatch_read(&request, &mut response, &mut queue),
                Err(errno) => errno,
            };
            syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap,
                (&response as *const InputdReadResponse) as u64,
                size_of::<InputdReadResponse>() as u64,
            )
        } else {
            let mut response = InputdIpcResponse {
                version: INPUTD_IPC_ABI_VERSION,
                op: request.op,
                ..InputdIpcResponse::default()
            };
            response.status = match validate(received as usize, &request) {
                Ok(()) => dispatch(&request, &mut response, &mut queue),
                Err(errno) => errno,
            };
            syscall3(
                SYS_RUSTOS_IPC_REPLY,
                reply_cap,
                (&response as *const InputdIpcResponse) as u64,
                size_of::<InputdIpcResponse>() as u64,
            )
        };
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "inputd: reply failed errno={}", -reply);
        }
    }
}

fn dispatch(
    request: &InputdIpcRequest,
    response: &mut InputdIpcResponse,
    queue: &mut InputQueue,
) -> i32 {
    match request.op {
        INPUTD_IPC_OP_PING => {
            response.approved_len = request.requested_len;
            0
        }
        INPUTD_IPC_OP_STATS => match fetch_stats(queue) {
            Ok(stats) => {
                response.stats = stats;
                0
            }
            Err(errno) => errno,
        },
        INPUTD_IPC_OP_AUTHORIZE_READ => authorize_read(request, response, queue),
        INPUTD_IPC_OP_DRAIN_INGEST => match drain_ingest(queue) {
            Ok(count) => {
                response.approved_len = count as u64;
                match fetch_stats(queue) {
                    Ok(stats) => response.stats = stats,
                    Err(errno) => return errno,
                }
                0
            }
            Err(errno) => errno,
        },
        _ => libc::EINVAL,
    }
}

fn dispatch_read(
    request: &InputdIpcRequest,
    response: &mut InputdReadResponse,
    queue: &mut InputQueue,
) -> i32 {
    if request.pid == 0 || request.tid == 0 || request.fd > i32::MAX as u64 {
        return libc::EINVAL;
    }
    if let Err(errno) = drain_ingest(queue) {
        return errno;
    }
    let requested = request
        .requested_len
        .min(INPUTD_READ_PAYLOAD_CAPACITY as u64) as usize;
    let status = match request.access {
        INPUTD_ACCESS_NATIVE => fill_native_payload(queue, &mut response.payload, requested),
        INPUTD_ACCESS_EVDEV => fill_evdev_payload(queue, &mut response.payload, requested),
        _ => return libc::EINVAL,
    };
    let Ok(len) = status else {
        return status.err().unwrap_or(libc::EINVAL);
    };
    response.payload_len = len as u32;
    match fetch_stats(queue) {
        Ok(stats) => response.stats = stats,
        Err(errno) => return errno,
    }
    0
}

fn drain_ingest(queue: &mut InputQueue) -> Result<usize, i32> {
    let mut events = vec![InputIngressWire::default(); INPUTD_INGEST_MAX_EVENTS];
    let mut count = 0_u32;
    let args = InputIngestBrokerArgs {
        abi_version: INPUTD_IPC_ABI_VERSION,
        reserved0: 0,
        reserved1: 0,
        out_events_ptr: events.as_mut_ptr() as u64,
        out_capacity: events.len() as u32,
        reserved2: 0,
        out_count_ptr: (&mut count as *mut u32) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_INPUT_INGEST_BROKER,
        (&args as *const InputIngestBrokerArgs) as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    let count = (count as usize).min(events.len());
    for wire in events.iter().take(count) {
        if wire.access == INPUTD_ACCESS_NATIVE {
            queue.push(wire.event);
        }
    }
    if count > 0 && !INPUT_DELIVERY_LOGGED.swap(true, Ordering::AcqRel) {
        debug_line(&format!(
            "input: pointer event delivered kind=inputd-drain count={count}"
        ));
    }
    Ok(count)
}

fn fill_native_payload(
    queue: &mut InputQueue,
    payload: &mut [u8; INPUTD_READ_PAYLOAD_CAPACITY],
    requested: usize,
) -> Result<usize, i32> {
    let event_size = size_of::<input_evdev::InputEvent>();
    let capacity = (requested / event_size).min(queue.len());
    if capacity == 0 {
        return Ok(0);
    }
    let mut written = 0usize;
    for _ in 0..capacity {
        let Some(event) = queue.pop_front() else {
            break;
        };
        let bytes = unsafe {
            slice::from_raw_parts(
                (&event as *const input_evdev::InputEvent).cast::<u8>(),
                event_size,
            )
        };
        payload[written..written + event_size].copy_from_slice(bytes);
        written += event_size;
    }
    Ok(written)
}

fn fill_evdev_payload(
    queue: &mut InputQueue,
    payload: &mut [u8; INPUTD_READ_PAYLOAD_CAPACITY],
    requested: usize,
) -> Result<usize, i32> {
    let event_size = size_of::<input_evdev::LinuxInputEvent>();
    let output_capacity = requested / event_size;
    if output_capacity < input_evdev::MAX_EVDEV_EVENTS_PER_INPUT_EVENT || queue.is_empty() {
        return Ok(0);
    }
    let mut input = Vec::new();
    let max_input = (output_capacity / input_evdev::MAX_EVDEV_EVENTS_PER_INPUT_EVENT)
        .min(queue.len())
        .min(input_evdev::MAX_INPUT_EVENTS_PER_READ);
    for _ in 0..max_input {
        if let Some(event) = queue.pop_front() {
            input.push(event);
        }
    }
    let mut output = vec![input_evdev::LinuxInputEvent::default(); output_capacity];
    let written_events = input_evdev::translate_input_events_to_evdev(&input, &mut output)
        .map_err(|_| libc::EINVAL)?;
    let bytes_len = written_events * event_size;
    let bytes = unsafe { slice::from_raw_parts(output.as_ptr().cast::<u8>(), bytes_len) };
    payload[..bytes_len].copy_from_slice(bytes);
    Ok(bytes_len)
}

fn authorize_read(
    request: &InputdIpcRequest,
    response: &mut InputdIpcResponse,
    queue: &InputQueue,
) -> i32 {
    if request.pid == 0 || request.tid == 0 || request.fd > i32::MAX as u64 {
        return libc::EINVAL;
    }
    let max_len = match request.access {
        INPUTD_ACCESS_NATIVE => INPUTD_MAX_NATIVE_READ_BYTES,
        INPUTD_ACCESS_EVDEV => INPUTD_MAX_EVDEV_READ_BYTES,
        _ => return libc::EINVAL,
    };
    response.approved_len = request.requested_len.min(max_len);
    match fetch_stats(queue) {
        Ok(stats) => response.stats = stats,
        Err(errno) => return errno,
    }
    0
}

fn fetch_stats(queue: &InputQueue) -> Result<InputStatsWire, i32> {
    let mut stats = InputStatsWire::default();
    let args = InputStatsBrokerArgs {
        abi_version: 1,
        reserved0: 0,
        reserved1: 0,
        out_stats_ptr: (&mut stats as *mut InputStatsWire) as u64,
    };
    let result = syscall1(
        SYS_RUSTOS_INPUT_STATS_BROKER,
        (&args as *const InputStatsBrokerArgs) as u64,
    );
    if result < 0 {
        return Err(last_errno());
    }
    stats.queued = stats.queued.saturating_add(queue.len() as u64);
    stats.dropped_lossy = stats.dropped_lossy.saturating_add(queue.dropped_lossy);
    Ok(stats)
}

fn validate(received: usize, request: &InputdIpcRequest) -> Result<(), i32> {
    if received != size_of::<InputdIpcRequest>()
        || request.version != INPUTD_IPC_ABI_VERSION
        || request.flags & !INPUTD_READ_FLAG_NONBLOCK != 0
        || request.reserved0 != 0
        || request.reserved1 != 0
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        INPUTD_IPC_OP_PING => Ok(()),
        INPUTD_IPC_OP_STATS => Ok(()),
        INPUTD_IPC_OP_AUTHORIZE_READ => Ok(()),
        INPUTD_IPC_OP_DRAIN_INGEST => Ok(()),
        INPUTD_IPC_OP_READ => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
