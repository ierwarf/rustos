use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn exercise_gate(release: bool) {
    let marker = std::env::temp_dir().join(format!(
        "rustos-hostd-gate-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let (mut parent, child_gate) = UnixStream::pair().unwrap();
    let child_fd = child_gate.as_raw_fd();
    let parent_fd = parent.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rustos-hostd"));
    command
        .arg("__exec-gated")
        .arg(child_fd.to_string())
        .arg("/usr/bin/touch")
        .arg(&marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            libc::close(parent_fd);
            if libc::fcntl(child_fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(child_gate);
    assert!(child.try_wait().unwrap().is_none());
    assert!(!marker.exists());
    if release {
        parent.write_all(&[1]).unwrap();
    }
    drop(parent);
    assert_eq!(child.wait().unwrap().success(), release);
    assert_eq!(marker.exists(), release);
    if release {
        fs::remove_file(marker).unwrap();
    }
}

#[test]
fn gated_wrapper_denies_exec_on_eof() {
    exercise_gate(false);
}

#[test]
fn gated_wrapper_executes_only_after_release() {
    exercise_gate(true);
}
