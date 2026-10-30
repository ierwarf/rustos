//! Loader receive custody and optional, descriptor-based job placement.
//! The kernel stamps sender identity separately; request bytes never name a
//! receiver-local descriptor. After prepare captures the exact JobId, loaderd
//! closes the received descriptor on every request outcome.
use rustos_user_abi::syscall::LOADER_OP_SPAWN_EXEC;

pub fn job_handle(op: u16, descriptors: &[i32]) -> Result<Option<u64>, i32> {
    match descriptors {
        [] => Ok(None),
        [fd] if op == LOADER_OP_SPAWN_EXEC && *fd >= 0 => Ok(Some(*fd as u64)),
        _ => Err(22),
    }
}

/// Outputs are valid only after a successful receive. Error output bytes must
/// never be mistaken for a published descriptor and closed by the service.
pub struct ReceivedHandles {
    pub fds: [i32; 1],
    pub count: u16,
}

impl Default for ReceivedHandles {
    fn default() -> Self { Self { fds: [-1], count: 0 } }
}

impl ReceivedHandles {
    pub fn clear(&mut self) { self.count = 0; }
    pub fn finish(&mut self, result: i64) -> i64 {
        if result < 0 { self.clear(); }
        result
    }
    pub fn as_slice(&self) -> &[i32] { &self.fds[..usize::from(self.count)] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::{LOADER_OP_ACTIVATE, LOADER_OP_EXEC_TARGET};

    #[test]
    fn placement_is_one_transferred_descriptor_and_never_exec_authority() {
        assert_eq!(job_handle(LOADER_OP_SPAWN_EXEC, &[]), Ok(None));
        assert_eq!(job_handle(LOADER_OP_SPAWN_EXEC, &[0]), Ok(Some(0)));
        assert_eq!(job_handle(LOADER_OP_SPAWN_EXEC, &[9]), Ok(Some(9)));
        for op in [LOADER_OP_ACTIVATE, LOADER_OP_EXEC_TARGET, u16::MAX] {
            assert_eq!(job_handle(op, &[]), Ok(None));
            assert_eq!(job_handle(op, &[9]), Err(22));
        }
        assert_eq!(job_handle(LOADER_OP_SPAWN_EXEC, &[-1]), Err(22));
        assert_eq!(job_handle(LOADER_OP_SPAWN_EXEC, &[8, 9]), Err(22));
    }

    #[test]
    fn loader_receive_keeps_sender_authentication_and_forwards_job_custody() {
        // Source witness; the binary is typechecked by the signed build lane.
        let source = include_str!("main.rs");
        assert!(source.contains("ipc::recv_with_handles_and_sender(&args)"));
        assert!(source.contains("ipc::reply_recv_with_handles_and_sender(&args)"));
        let request = source.split_once("fn handle_request(").unwrap().1;
        assert!(request.find("request.requester_is_exact_sender(sender_pid)").unwrap()
            < request.find("loaderd::placement::job_handle(request.op, descriptors)").unwrap());
        assert!(request.contains("job_handle: job_handle.unwrap_or(0)"));
        let serve = source.split_once("fn serve(endpoint: u64)").unwrap().1;
        assert!(serve.find("handle_wire_request(received as usize").unwrap()
            < serve.find("close_fds(handles.as_slice())").unwrap());
    }

    #[test]
    fn failed_receive_outputs_never_become_owned_descriptors() {
        let mut received = ReceivedHandles { fds: [9], count: 1 };
        assert_eq!(received.finish(-14), -14);
        assert!(received.as_slice().is_empty());
        received.count = 1;
        assert_eq!(received.finish(128), 128);
        assert_eq!(received.as_slice(), &[9]);
        received.clear();
        assert!(received.as_slice().is_empty());
    }
}
