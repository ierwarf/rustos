use rustos_svc_runtime::syscall::syscall1;
use rustos_user_abi::syscall::{
    LoaderSpawnRequest, RustosProcCommitBrokerArgs, RustosProcExecTargetBrokerArgs,
    LOADER_OP_EXEC_TARGET, LOADER_OP_SPAWN_EXEC, PROC_BROKER_ABI_VERSION, PROC_BROKER_FORMAT_ELF64,
    PROC_BROKER_FORMAT_PE64, SYS_RUSTOS_PROC_COMMIT_BROKER, SYS_RUSTOS_PROC_EXEC_TARGET_BROKER,
};

use crate::EINVAL;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoaderOperation {
    SpawnExec,
    ExecTarget,
}

impl LoaderOperation {
    pub(crate) fn from_op(op: u16) -> Result<Self, i32> {
        match op {
            LOADER_OP_SPAWN_EXEC => Ok(Self::SpawnExec),
            LOADER_OP_EXEC_TARGET => Ok(Self::ExecTarget),
            _ => Err(EINVAL),
        }
    }

    pub(crate) fn allows_format(self, format: u16) -> bool {
        match self {
            Self::SpawnExec => matches!(format, PROC_BROKER_FORMAT_ELF64 | PROC_BROKER_FORMAT_PE64),
            Self::ExecTarget => format == PROC_BROKER_FORMAT_ELF64,
        }
    }
}

pub(crate) fn commit_prepared_executable(
    operation: LoaderOperation,
    request: &LoaderSpawnRequest,
    prepare_handle: u64,
    exec_path_ptr: u64,
    exec_path_len: u64,
    argv_ptr: u64,
    envp_ptr: u64,
) -> i64 {
    match operation {
        LoaderOperation::SpawnExec => {
            let args = RustosProcCommitBrokerArgs {
                prepare_handle,
                exec_path_ptr,
                exec_path_len,
                argv_ptr,
                envp_ptr,
                flags: request.flags as u64,
                console_session: request.console_session,
                weight_micros: request.weight_micros,
                requester_pid: request.requester_pid,
            };
            unsafe {
                syscall1(
                    SYS_RUSTOS_PROC_COMMIT_BROKER,
                    (&args as *const RustosProcCommitBrokerArgs) as u64,
                )
            }
        }
        LoaderOperation::ExecTarget => {
            let args = RustosProcExecTargetBrokerArgs {
                abi_version: PROC_BROKER_ABI_VERSION,
                flags: request.flags,
                prepare_handle,
                exec_ticket: request.exec_ticket,
                target_pid: request.target_pid,
                target_tid: request.target_tid,
                exec_path_ptr,
                exec_path_len,
                argv_ptr,
                envp_ptr,
                console_session: request.console_session,
                weight_micros: request.weight_micros,
                requester_pid: request.requester_pid,
                ..RustosProcExecTargetBrokerArgs::default()
            };
            unsafe {
                syscall1(
                    SYS_RUSTOS_PROC_EXEC_TARGET_BROKER,
                    (&args as *const RustosProcExecTargetBrokerArgs) as u64,
                )
            }
        }
    }
}
