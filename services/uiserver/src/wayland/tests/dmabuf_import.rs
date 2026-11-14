//! Object-level `zwp_linux_dmabuf_v1` import tests over the real Wayland wire.
//!
//! A genuine `DeviceBuffer` capability only exists inside the kernel, so the
//! kernel observations (`fstat` identity, one-byte liveness probe) are
//! substituted through `super::super::dmabuf::with_test_fd_ops`; everything between the wire
//! and those two observations is the production path.
use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

const DMABUF: u32 = 3;
const ARGB8888: u32 = 0x3432_5241;
const INVALID_WL_BUFFER: u32 = 7;
const OUT_OF_BOUNDS: u32 = 6;
const ALREADY_USED: u32 = 0;

struct Fixture {
    display: wayland_server::Display<super::super::WaylandState>,
    state: super::super::WaylandState,
    peer: UnixStream,
    /// Client ids must be allocated densely, so every new object takes the next one.
    next_id: u32,
}

fn fixture() -> Fixture {
    let mut display = test_display();
    super::super::create_wayland_globals(&display.handle());
    let mut state = super::super::WaylandState::new(1024, 768);
    let (server, mut peer) = UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(super::super::WaylandClientState::default()))
        .unwrap();
    wire::send(&mut peer, &wire::message(1, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    let mut name = None;
    for (object, opcode, payload) in wire::events(&mut peer) {
        if object == 2 && opcode == 0 {
            let len = u32::from_ne_bytes(payload[4..8].try_into().unwrap()) as usize;
            if &payload[8..8 + len - 1] == b"zwp_linux_dmabuf_v1" {
                name = Some(u32::from_ne_bytes(payload[0..4].try_into().unwrap()));
            }
        }
    }
    let mut args = wire::u32s(&[name.expect("dmabuf global")]);
    args.extend(wire::string("zwp_linux_dmabuf_v1"));
    args.extend(wire::u32s(&[3, DMABUF]));
    wire::send(&mut peer, &wire::message(2, 0, &args));
    pump(&mut display, &mut state);
    wire::events(&mut peer);
    Fixture { display, state, peer, next_id: 4 }
}

impl Fixture {
    fn create_params(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        wire::send(&mut self.peer, &wire::message(DMABUF, 1, &wire::u32s(&[id])));
        id
    }
    fn next_object(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    fn add(&mut self, params: u32, fd: &impl AsRawFd, stride: u32) {
        wire::send_fd(
            &self.peer,
            &wire::message(params, 1, &wire::u32s(&[0, 0, stride, 0, 0])),
            fd.as_raw_fd(),
        );
    }
    fn create(&mut self, params: u32, w: u32, h: u32) {
        wire::send(&mut self.peer, &wire::message(params, 2, &wire::u32s(&[w, h, ARGB8888, 0])));
    }
    fn create_immed(&mut self, params: u32, id: u32, w: u32, h: u32) {
        wire::send(
            &mut self.peer,
            &wire::message(params, 3, &wire::u32s(&[id, w, h, ARGB8888, 0])),
        );
    }
    fn destroy(&mut self, params: u32) {
        wire::send(&mut self.peer, &wire::message(params, 0, &[]));
    }
    fn events(&mut self) -> Vec<(u32, u16, Vec<u8>)> {
        pump(&mut self.display, &mut self.state);
        wire::events(&mut self.peer)
    }
}

fn memfd(len: usize) -> std::fs::File {
    use std::os::fd::FromRawFd;
    let name = std::ffi::CString::new("forged").unwrap();
    // SAFETY: fresh descriptor owned by the returned `File`.
    let file = unsafe {
        let fd = libc::memfd_create(name.as_ptr(), 0);
        assert!(fd >= 0);
        std::fs::File::from_raw_fd(fd)
    };
    file.set_len(len as u64).unwrap();
    file
}

fn display_error(events: &[(u32, u16, Vec<u8>)]) -> Option<(u32, u32)> {
    events.iter().find(|(object, opcode, _)| *object == 1 && *opcode == 0).map(|(_, _, p)| {
        (
            u32::from_ne_bytes(p[0..4].try_into().unwrap()),
            u32::from_ne_bytes(p[4..8].try_into().unwrap()),
        )
    })
}

fn is_event(events: &[(u32, u16, Vec<u8>)], object: u32, opcode: u16) -> bool {
    events.iter().any(|(o, c, _)| *o == object && *c == opcode)
}

fn genuine_identity(fd: std::os::fd::RawFd) -> Option<super::super::dmabuf::FdIdentity> {
    // SAFETY: plain-data stat filled by fstat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(super::super::dmabuf::FdIdentity {
        mode: libc::S_IFCHR | 0o600,
        rdev: rustos_user_abi::syscall::APP_BUFFER_STAT_RDEV,
        size: st.st_size as u64,
    })
}

fn live(_: std::os::fd::RawFd, _: usize) -> bool {
    true
}
fn revoked(_: std::os::fd::RawFd, _: usize) -> bool {
    false
}

#[test]
fn forged_plain_fd_is_refused_by_both_create_paths() {
    let mut f = fixture();
    let forged = memfd(1 << 16);
    let params = f.create_params();
    f.add(params, &forged, 256);
    f.create(params, 8, 8);
    let events = f.events();
    assert!(is_event(&events, params, 1), "create with a forged fd must answer failed: {events:?}");
    assert!(!is_event(&events, params, 0));

    let params = f.create_params();
    f.add(params, &forged, 256);
    let buffer = f.next_object();
    f.create_immed(params, buffer, 8, 8);
    let events = f.events();
    assert_eq!(display_error(&events), Some((params, INVALID_WL_BUFFER)), "{events:?}");
}

#[test]
fn genuine_capability_imports_once_and_params_cannot_be_reused() {
    let mut f = fixture();
    let cap = memfd(1 << 16);
    super::super::dmabuf::with_test_fd_ops(genuine_identity, live, || {
        let params = f.create_params();
        f.add(params, &cap, 256);
        f.create(params, 8, 8);
        let events = f.events();
        assert!(is_event(&events, params, 0), "genuine capability must yield created: {events:?}");
        assert!(!is_event(&events, params, 1));

        // Double import through the same params object.
        f.create(params, 8, 8);
        let events = f.events();
        assert_eq!(display_error(&events), Some((params, ALREADY_USED)), "{events:?}");
    });
}

#[test]
fn revoked_or_stale_capability_fails_the_liveness_probe() {
    let mut f = fixture();
    let cap = memfd(1 << 16);
    super::super::dmabuf::with_test_fd_ops(genuine_identity, revoked, || {
        let params = f.create_params();
        f.add(params, &cap, 256);
        f.create(params, 8, 8);
        let events = f.events();
        assert!(is_event(&events, params, 1) && !is_event(&events, params, 0), "{events:?}");

        let params = f.create_params();
        f.add(params, &cap, 256);
        let buffer = f.next_object();
        f.create_immed(params, buffer, 8, 8);
        let events = f.events();
        assert_eq!(display_error(&events), Some((params, INVALID_WL_BUFFER)), "{events:?}");
    });
}

#[test]
fn geometry_beyond_the_capability_is_out_of_bounds() {
    let mut f = fixture();
    let cap = memfd(4096);
    super::super::dmabuf::with_test_fd_ops(genuine_identity, live, || {
        let params = f.create_params();
        f.add(params, &cap, 256);
        let buffer = f.next_object();
        f.create_immed(params, buffer, 64, 64);
        let events = f.events();
        assert_eq!(display_error(&events), Some((params, OUT_OF_BOUNDS)), "{events:?}");
    });
}

#[test]
fn live_params_objects_are_bounded_per_client() {
    let mut f = fixture();
    for _ in 0..super::super::dmabuf::MAX_WAYLAND_DMABUF_PARAMS_PER_CLIENT {
        f.create_params();
    }
    let events = f.events();
    assert!(display_error(&events).is_none(), "the bound itself is admitted: {events:?}");
    f.create_params();
    let events = f.events();
    assert_eq!(display_error(&events).map(|(object, _)| object), Some(DMABUF), "{events:?}");
}

#[test]
fn imported_buffers_are_bounded_per_client() {
    let mut f = fixture();
    let cap = memfd(1 << 16);
    let limit = super::super::MAX_WAYLAND_SHM_POOLS_PER_CLIENT;
    super::super::dmabuf::with_test_fd_ops(genuine_identity, live, || {
        for index in 0..=limit {
            let params = f.create_params();
            f.add(params, &cap, 256);
            f.create(params, 8, 8);
            f.destroy(params);
            let events = f.events();
            if index < limit {
                assert!(is_event(&events, params, 0), "import {index} must succeed: {events:?}");
            } else {
                assert!(
                    is_event(&events, params, 1),
                    "import beyond the per-client bound must fail: {events:?}"
                );
            }
        }
    });
}
