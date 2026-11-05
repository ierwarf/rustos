//! Private streaming snapshot sink. No descriptor is published before sealing.
//! The byte reservation follows the owned descriptor through the terminal reply.
use super::*;
use storage_core::file_io::WriteAt;
use vfsd::snapshot_budget::{SnapshotReservation, snapshot_reservation_bytes, SNAPSHOT_IN_FLIGHT_BYTES};

static SNAPSHOT_BUDGET: vfsd::snapshot_budget::SnapshotBudget =
    vfsd::snapshot_budget::SnapshotBudget::new(SNAPSHOT_IN_FLIGHT_BYTES);

type Lease = vfsd::ExecutableSnapshotLease<fn(i32)>;
pub(crate) struct ExecutableSnapshotOpen {
    lease: Lease,
    _reservation: Option<SnapshotReservation<'static>>,
}
impl ExecutableSnapshotOpen {
    pub(crate) fn duplicate_cached(fd: i32, bytes: u64, generation: u64, maximum: u64,
        duplicate: fn(i32) -> Result<i32, i32>, close: fn(i32)) -> Result<Self, i32> {
        Lease::duplicate_cached(fd, bytes, generation, maximum, duplicate, close)
            .map(|lease| Self { lease, _reservation: None })
    }
    pub(crate) fn fd(&self) -> i32 { self.lease.fd() }
    pub(crate) fn file_bytes(&self) -> u64 { self.lease.file_bytes() }
    pub(crate) fn mount_generation(&self) -> u64 { self.lease.mount_generation() }
}

pub(crate) struct SnapshotWriter {
    fd: i32,
    written: u64,
    write_ns: u64,
    len: u64,
    reservation: Option<SnapshotReservation<'static>>,
}
impl SnapshotWriter {
    pub(crate) fn new(len: u64) -> Result<Self, i32> {
        let cost = snapshot_reservation_bytes(len).ok_or(EOVERFLOW)?;
        let reservation = SNAPSHOT_BUDGET.reserve(cost).ok_or(ENOMEM)?;
        let name = CString::new("vfsd-executable-snapshot").map_err(|_| EINVAL)?;
        // SAFETY: the terminated name remains live through the syscall; the fd
        // is private to this owner until all four seals have succeeded.
        let fd = unsafe { rustos_svc_runtime::syscall::syscall2(
            linux_abi::SYS_MEMFD_CREATE, name.as_ptr() as u64,
            linux_abi::MFD_CLOEXEC | linux_abi::MFD_ALLOW_SEALING,
        ) };
        if fd < 0 { return Err((-fd) as i32); }
        let fd = match i32::try_from(fd) {
            Ok(fd) => fd,
            Err(_) => {
                // SAFETY: creation returned this still-private descriptor.
                let _ = unsafe { rustos_svc_runtime::syscall::syscall1(linux_abi::SYS_CLOSE, fd as u64) };
                return Err(EOVERFLOW);
            }
        };
        Ok(Self { fd, written: 0, write_ns: 0, len, reservation: Some(reservation) })
    }
    pub(crate) fn write_ns(&self) -> u64 { self.write_ns }
    pub(crate) fn seal(mut self, generation: u64) -> Result<ExecutableSnapshotOpen, i32> {
        if self.written != self.len { return Err(EIO); }
        let seals = linux_abi::F_SEAL_WRITE | linux_abi::F_SEAL_GROW
            | linux_abi::F_SEAL_SHRINK | linux_abi::F_SEAL_SEAL;
        // SAFETY: only this builder owns the fd; no writable mapping or other
        // descriptor has been published. Sealing failure leaves Drop in charge.
        let status = unsafe { rustos_svc_runtime::syscall::syscall3(
            linux_abi::SYS_FCNTL, self.fd as u64, linux_abi::F_ADD_SEALS, seals,
        ) };
        if status < 0 { return Err((-status) as i32); }
        let fd = self.fd;
        self.fd = -1;
        Ok(ExecutableSnapshotOpen {
            lease: Lease::from_owned(fd, self.len, generation, close_fd as fn(i32)),
            _reservation: self.reservation.take(),
        })
    }
}
impl WriteAt for SnapshotWriter {
    type Error = i32;
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, i32> {
        // This sink is deliberately private and sequential. Unlike a public
        // write_at emulation, no other open description can race this cursor.
        if offset != self.written || offset.checked_add(bytes.len() as u64).is_none_or(|end| end > self.len) {
            return Err(EINVAL);
        }
        // SAFETY: the input slice lives for the synchronous bounded write and
        // the descriptor remains exclusively owned by this builder.
        let started = rustos_svc_runtime::syscall::monotonic_nanos();
        let status = unsafe { rustos_svc_runtime::syscall::syscall3(
            linux_abi::SYS_WRITE, self.fd as u64, bytes.as_ptr() as u64, bytes.len() as u64,
        ) };
        self.write_ns = self.write_ns.saturating_add(rustos_svc_runtime::syscall::monotonic_nanos().saturating_sub(started));
        if status < 0 { return Err((-status) as i32); }
        let n = usize::try_from(status).map_err(|_| EOVERFLOW)?;
        if n > bytes.len() { return Err(EIO); }
        self.written = self.written.checked_add(n as u64).ok_or(EOVERFLOW)?;
        Ok(n)
    }
}
impl Drop for SnapshotWriter {
    fn drop(&mut self) { if self.fd >= 0 { close_fd(self.fd); } }
}
