//! Explicit developer image-admission probe, not application startup.
//! Runs the SAME loader work protocol from an existing authorized client;
//! VALIDATE_IMAGE never prepares/maps/commits/activates the target process.
use core::sync::atomic::{AtomicBool, Ordering};
use rustos_user_abi::syscall::*;
static STARTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn after_launch(exec: &str) {
    let Some(path) = option_env!("RUSTOS_RUNTIME_IMAGE_PROBE") else { return; };
    if exec.trim_start_matches('/') != "apps/wayclick/wayclick.elf" || STARTED.swap(true, Ordering::AcqRel) { return; }
    let spawned = std::thread::Builder::new().name(String::from("image-admission-probe")).spawn(move || {
        let result = run(path);
        match result {
            Ok(reply) => rustos_svc_runtime::ipc::debug_line(&format!(
                "runtimed: staged-image-probe admitted exec={path} bytes={} format={}", reply.file_bytes, reply.image_format)),
            Err(errno) => rustos_svc_runtime::ipc::debug_line(&format!(
                "runtimed: staged-image-probe refused exec={path} errno={errno}")),
        }
    });
    if let Err(error) = spawned {
        rustos_svc_runtime::ipc::debug_line(&format!("runtimed: staged-image-probe thread refused error={error}"));
    }
}
fn run(path: &str) -> Result<LoaderSpawnResponse, i32> {
    // SAFETY: this pointer-free call only demotes the diagnostic's own thread.
    let demote = unsafe { rustos_svc_runtime::syscall::syscall0(SYS_RUSTOS_SCHED_DEMOTE_SELF) };
    if demote != 0 { return Err(if demote < 0 { (-demote) as i32 } else { 22 }); }
    if path.is_empty() || path.len() > LOADER_SPAWN_EXEC_PATH_CAPACITY || path.as_bytes().contains(&0) { return Err(22); }
    let endpoint = rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_LOADERD);
    if endpoint <= 0 { return Err(if endpoint < 0 { (-endpoint) as i32 } else { 11 }); }
    let mut request = LoaderSpawnRequest {
        op: LOADER_OP_VALIDATE_IMAGE, requester_pid: u64::from(std::process::id()),
        exec_path_len: path.len() as u32,
        deadline_ns: rustos_svc_runtime::syscall::monotonic_nanos().saturating_add(LOADER_APPLICATION_WORK_MAX_LEASE_MS * 1_000_000),
        ..LoaderSpawnRequest::default()
    };
    request.exec_path[..path.len()].copy_from_slice(path.as_bytes());
    let response = rustos_svc_runtime::loader_client::call(endpoint as u64, &request, None)?;
    if response.status != 0 { return Err(response.status); }
    Ok(response)
}
