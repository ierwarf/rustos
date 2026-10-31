//! Actual provider -> rootd incarnation health witness.
//!
//! This is slow periodic policy IPC from sessiond's control loop, never
//! part of terminal input/output, IPC reply, futex or scheduler decisions.
//! Kernel-stamped sender identity and rootd's exact lease graph are the
//! authority; our sequence rejects delayed replies and repeated evidence.
use std::mem::size_of;
use rustos_component_model::health_wire::{self, Pulse};
use std::time::{Duration, Instant};
use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR,
    COMMERCIAL_MAX_ROOTD_OP_SERVICE_HEARTBEAT, IPC_SERVICE_ROOTD,
    IPC_SERVICE_SESSIOND, SYS_RUSTOS_IPC_CALL_BOUNDED,
};
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_CALL_TIMEOUT_MS: u64 = 250;

pub(super) struct Heartbeat {
    due: Instant,
    sequence: u64,
    pub(super) failures: u32,
}

impl Heartbeat {
    pub(super) fn new() -> Self {
        Self { due: Instant::now(), sequence: 0, failures: 0 }
    }

    pub(super) fn poll(&mut self) {
        if Instant::now() < self.due {
            return;
        }
        self.due = Instant::now() + HEARTBEAT_INTERVAL;
        let Some(next) = self.sequence.checked_add(1) else {
            panic!("runtimed: heartbeat sequence exhaustion");
        };
        // A refused probe does not imply service loss. The root supervisor
        // knows whether our exact old PID is live, and only times out health
        // *after* the first authenticated probe for this incarnation.
        if self.send(next).is_ok() {
            self.sequence = next;
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
        }
    }

    fn send(&self, sequence: u64) -> Result<(), i32> {
        let endpoint = super::spawn::lookup_service_endpoint(IPC_SERVICE_ROOTD);
        if endpoint <= 0 {
            return Err(libc::EAGAIN);
        }
        let pid = unsafe { libc::syscall(libc::SYS_getpid as libc::c_long) };
        let tid = unsafe { libc::syscall(libc::SYS_gettid as libc::c_long) };
        if pid <= 0 || tid <= 0 {
            return Err(libc::ESRCH);
        }
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
        request.header.protocol = COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR;
        request.header.op = COMMERCIAL_MAX_ROOTD_OP_SERVICE_HEARTBEAT;
        request.header.subject_pid = pid as u64;
        request.header.subject_tid = tid as u64;
        let pulse = Pulse {
            service: IPC_SERVICE_SESSIOND,
            incarnation_pid: pid as u64,
            sequence,
        };
        let wire = health_wire::encode_pulse(pulse)
            .map_err(|_| libc::EINVAL)?;
        if wire.len() > request.payload.len() {
            return Err(libc::E2BIG);
        }
        request.payload[..wire.len()].copy_from_slice(&wire);
        request.payload_len = wire.len() as u32;
        let mut reply = CommercialMaxProtocolResponse::default();
        let bytes = unsafe {
            rustos_svc_runtime::syscall::syscall6(
                SYS_RUSTOS_IPC_CALL_BOUNDED,
                endpoint as u64,
                (&request as *const CommercialMaxProtocolRequest) as u64,
                size_of::<CommercialMaxProtocolRequest>() as u64,
                (&mut reply as *mut CommercialMaxProtocolResponse) as u64,
                size_of::<CommercialMaxProtocolResponse>() as u64,
                HEARTBEAT_CALL_TIMEOUT_MS,
            )
        };
        if bytes < 0 {
            return Err((-bytes) as i32);
        }
        if bytes as usize != size_of::<CommercialMaxProtocolResponse>()
            || !reply.is_valid_envelope_for(&request)
            || reply.status != 0 || reply.value0 != 0
            || reply.value1 == 0 || reply.descriptor_count != 0
        {
            return Err(libc::EPROTO);
        }
        health_wire::decode_accept(
            &reply.payload[..reply.payload_len as usize], pulse,
        ).map_err(|_| libc::EPROTO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heartbeat_is_never_sent_per_console_io() {
        assert!(HEARTBEAT_INTERVAL >= Duration::from_secs(1));
        assert!(HEARTBEAT_CALL_TIMEOUT_MS < 1_000);
        let watch = Heartbeat::new();
        assert_eq!(watch.sequence, 0);
    }
}
