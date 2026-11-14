use super::{
    can_retain_undamaged_buffer, checked_wayland_pixel_count, claim_wayland_rearm,
    copy_wayland_bgra_row, transient_wayland_readiness_error, valid_wayland_pool_resize,
    validate_wayland_buffer_layout, wayland_nonnegative_i32, WaylandWindowSnapshot,
    MAX_WAYLAND_BUFFER_DIMENSION, MAX_WAYLAND_SHM_POOL_BYTES,
};
use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn new_wayland_window_continues_the_console_cascade() {
    let mut state = super::WaylandState::new(2560, 1440);
    let first = state.allocate_window_frame();
    // With three console windows already placed, the next window must
    // start after them rather than at the desktop origin they began from.
    state.cascade_base = 3;
    let after_consoles = state.allocate_window_frame();
    assert!(after_consoles.x > first.x && after_consoles.y > first.y);
    let console_three = crate::layout::default_console_window_rect(2560, 1440, 3);
    assert_eq!(
        (after_consoles.x, after_consoles.y),
        (console_three.x, console_three.y)
    );
}

#[test]
fn wayland_readiness_requires_one_dispatch_before_rearm() {
    let needs_rearm = AtomicBool::new(true);
    assert!(claim_wayland_rearm(&needs_rearm));
    assert!(!claim_wayland_rearm(&needs_rearm));
    needs_rearm.store(true, Ordering::Release);
    assert!(claim_wayland_rearm(&needs_rearm));
}

#[test]
fn wayland_readiness_retries_only_transient_transport_failures() {
    assert!(transient_wayland_readiness_error(ErrorKind::BrokenPipe));
    assert!(transient_wayland_readiness_error(ErrorKind::TimedOut));
    assert!(!transient_wayland_readiness_error(ErrorKind::InvalidInput));
}

use crate::canvas::Rect;
use std::sync::Arc;

#[test]
fn wayland_buffer_limits_reject_oversized_dimensions() {
    assert!(checked_wayland_pixel_count(MAX_WAYLAND_BUFFER_DIMENSION + 1, 1).is_none());
    assert!(checked_wayland_pixel_count(1, MAX_WAYLAND_BUFFER_DIMENSION + 1).is_none());
    assert!(checked_wayland_pixel_count(1920, 1080).is_some());
}

#[test]
fn wayland_buffer_layout_rejects_out_of_bounds_and_bad_stride() {
    assert!(validate_wayland_buffer_layout(0, 128, 128, 128 * 4, 128 * 128 * 4).is_some());
    assert!(validate_wayland_buffer_layout(0, 128, 128, 127 * 4, 128 * 128 * 4).is_none());
    assert!(validate_wayland_buffer_layout(
        MAX_WAYLAND_SHM_POOL_BYTES,
        128,
        128,
        128 * 4,
        MAX_WAYLAND_SHM_POOL_BYTES,
    )
    .is_none());
}

#[test]
fn wayland_pool_resize_never_shrinks_or_exceeds_limit() {
    assert!(!valid_wayland_pool_resize(128, 0));
    assert!(!valid_wayland_pool_resize(128, 127));
    assert!(valid_wayland_pool_resize(128, 128));
    assert!(valid_wayland_pool_resize(128, 129));
    assert!(valid_wayland_pool_resize(128, MAX_WAYLAND_SHM_POOL_BYTES));
    assert!(!valid_wayland_pool_resize(
        128,
        MAX_WAYLAND_SHM_POOL_BYTES + 1
    ));
}

#[test]
fn wayland_integer_args_reject_negative_values() {
    assert_eq!(wayland_nonnegative_i32(0), Some(0));
    assert_eq!(wayland_nonnegative_i32(42), Some(42));
    assert_eq!(wayland_nonnegative_i32(-1), None);
}

#[test]
fn wayland_argb_row_preserves_alpha() {
    let source = [1, 2, 3, 4, 5, 6, 7, 8];
    let mut pixels = [0_u32; 2];
    assert!(copy_wayland_bgra_row(&source, &mut pixels, true));
    assert_eq!(pixels, [0x0403_0201, 0x0807_0605]);
}

#[test]
fn wayland_xrgb_row_forces_opaque_alpha_across_chunk_boundary() {
    let mut source = [0_u8; 20];
    for (index, byte) in source.iter_mut().enumerate() {
        *byte = index as u8;
    }
    let mut pixels = [0_u32; 5];
    assert!(copy_wayland_bgra_row(&source, &mut pixels, false));
    assert_eq!(
        pixels,
        [
            0xff02_0100,
            0xff06_0504,
            0xff0a_0908,
            0xff0e_0d0c,
            0xff12_1110
        ]
    );
}

#[test]
fn undamaged_buffer_reuse_requires_an_exact_existing_layout() {
    assert!(can_retain_undamaged_buffer(
        true, 800, 520, 800, 800, 520, 800
    ));
    assert!(!can_retain_undamaged_buffer(
        false, 800, 520, 800, 800, 520, 800
    ));
    assert!(!can_retain_undamaged_buffer(
        true, 800, 520, 800, 801, 520, 801
    ));
}

#[test]
fn wayland_snapshot_equality_uses_content_version_not_pixels() {
    let mut first = WaylandWindowSnapshot {
        surface_id: 1,
        title: String::from("app"),
        frame: Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        },
        minimized: false,
        server_decoration: true,
        drag_icon: false,
        content_version: 7,
        damage: Rect::empty(),
        pixels: Arc::new(vec![1, 2, 3]),
        width: 3,
        height: 1,
        stride_pixels: 3,
    };
    let mut second = first.clone();
    second.pixels = Arc::new(vec![9, 9, 9]);
    assert_eq!(first, second);
    first.content_version = 8;
    assert_ne!(first, second);
}

fn test_ticket() -> super::LiveObjectTicket {
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    super::LiveObjectTicket::admit(&counter, usize::MAX, None).expect("ticket")
}

pub(super) fn test_display() -> wayland_server::Display<super::WaylandState> {
    wayland_server::Display::<super::WaylandState>::new().expect("display")
}

/// Client ids are only unique within one display, so tests that compare
/// clients must insert them into the same display.
pub(super) fn test_client(
    display: &wayland_server::Display<super::WaylandState>,
) -> wayland_server::backend::ClientId {
    use std::os::unix::net::UnixStream;
    let (server_end, _client_end) = UnixStream::pair().expect("socket pair");
    display
        .handle()
        .insert_client(
            server_end,
            std::sync::Arc::new(super::WaylandClientState::default()),
        )
        .expect("client")
        .id()
}

/// Register a surface without a protocol resource, the way a dispatch would
/// after `create_surface_data`.
fn register_surface(
    state: &mut super::WaylandState,
    client: &wayland_server::backend::ClientId,
) -> u32 {
    let surface_id = state.allocate_surface_id();
    state.surfaces.push(super::SurfaceData {
        shared: std::sync::Arc::new(std::sync::Mutex::new(super::WaylandSurfaceState {
            surface_id,
            client_id: Some(client.clone()),
            alive: true,
            ..Default::default()
        })),
    });
    surface_id
}

#[test]
fn surface_identity_is_unique_across_clients() {
    // Protocol object ids are per client, so two clients commonly own the same
    // wl_surface id; the compositor identity must still name exactly one.
    let display = test_display();
    let client_a = test_client(&display);
    let client_b = test_client(&display);
    let mut state = super::WaylandState::new(2560, 1440);
    let a = register_surface(&mut state, &client_a);
    let b = register_surface(&mut state, &client_b);
    assert_ne!(a, b);
    assert_ne!(a, 0);
    let found_a = state.find_surface_by_id(a).expect("a");
    let found_b = state.find_surface_by_id(b).expect("b");
    assert!(!std::sync::Arc::ptr_eq(&found_a, &found_b));
    assert_eq!(found_b.lock().unwrap().client_id.as_ref(), Some(&client_b));
    assert!(state.find_surface_by_id(0).is_none());
    assert!(state.find_surface_by_id(b + 100).is_none());
    // A retired surface can no longer be addressed.
    found_a.lock().unwrap().alive = false;
    assert!(state.find_surface_by_id(a).is_none());
}

#[test]
fn surface_identity_is_never_reissued_while_live() {
    let display = test_display();
    let client = test_client(&display);
    let mut state = super::WaylandState::new(2560, 1440);
    let first = register_surface(&mut state, &client);
    state.next_surface_id = first; // force the counter onto a live id
    let second = state.allocate_surface_id();
    assert_ne!(first, second);
    state.next_surface_id = u32::MAX;
    let wrapped = state.allocate_surface_id();
    assert_eq!(wrapped, u32::MAX);
    let after_wrap = state.allocate_surface_id();
    assert_ne!(after_wrap, 0, "zero is reserved for 'no surface'");
}

#[test]
fn one_client_cannot_exhaust_the_surface_table() {
    let display = test_display();
    let client_a = test_client(&display);
    let client_b = test_client(&display);
    let mut state = super::WaylandState::new(2560, 1440);
    for _ in 0..super::MAX_WAYLAND_SURFACES_PER_CLIENT {
        register_surface(&mut state, &client_a);
    }
    assert_eq!(
        state.client_surface_count(&client_a),
        super::MAX_WAYLAND_SURFACES_PER_CLIENT
    );
    assert_eq!(state.client_surface_count(&client_b), 0);
    const { assert!(super::MAX_WAYLAND_SURFACES_PER_CLIENT < super::MAX_WAYLAND_SURFACES) };
}

#[test]
fn surface_memory_budget_is_shared_across_surfaces() {
    use super::{
        surface_pixel_budget_allows, MAX_WAYLAND_BUFFER_PIXELS, MAX_WAYLAND_TOTAL_SURFACE_PIXELS,
    };
    assert!(surface_pixel_budget_allows(0, MAX_WAYLAND_BUFFER_PIXELS));
    assert!(surface_pixel_budget_allows(
        MAX_WAYLAND_TOTAL_SURFACE_PIXELS - 1,
        1
    ));
    assert!(!surface_pixel_budget_allows(
        MAX_WAYLAND_TOTAL_SURFACE_PIXELS,
        1
    ));
    assert!(
        !surface_pixel_budget_allows(usize::MAX, 1),
        "overflow must reject"
    );
    // Sixty-four maximum-size surfaces must not fit.
    assert!(MAX_WAYLAND_TOTAL_SURFACE_PIXELS < 64 * MAX_WAYLAND_BUFFER_PIXELS);
}

#[test]
fn retained_pixel_accounting_excludes_the_committing_surface() {
    let display = test_display();
    let client = test_client(&display);
    let mut state = super::WaylandState::new(2560, 1440);
    register_surface(&mut state, &client);
    register_surface(&mut state, &client);
    state.surfaces[0].shared.lock().unwrap().pixels = std::sync::Arc::new(vec![0; 100]);
    state.surfaces[1].shared.lock().unwrap().pixels = std::sync::Arc::new(vec![0; 40]);
    let first = state.surfaces[0].shared.clone();
    assert_eq!(state.retained_surface_pixels_excluding(&first), 40);
}

fn shm_file(bytes: &[u8]) -> std::fs::File {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::FileExt;
    let name = std::ffi::CString::new("uiserver-shm-regression").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.set_len(bytes.len() as u64).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    file
}

/// Plan and run a full snapshot the way a snapshot worker does.
fn copy_pixels(buffer: &super::BufferData) -> Option<(Arc<Vec<u32>>, usize, usize, usize)> {
    match buffer.plan_full()?.run().ok()? {
        super::SnapshotPixels::Full {
            pixels,
            width,
            height,
        } => Some((Arc::new(pixels), width, height, width)),
        super::SnapshotPixels::Damage { .. } => None,
    }
}

fn shm_buffer(
    pool: Arc<std::sync::Mutex<super::WaylandShmPoolState>>,
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
) -> super::BufferData {
    super::BufferData {
        shared: Arc::new(super::WaylandBufferState {
            pool,
            offset,
            width,
            height,
            stride,
            has_alpha: false,
            app_dmabuf: false,
            _ticket: test_ticket(),
        }),
    }
}

#[test]
fn shm_pool_rejects_undersized_backing_and_unseekable_fd() {
    use std::os::fd::OwnedFd;
    let file = shm_file(&[0; 4]);
    assert!(super::WaylandShmPoolState::new(OwnedFd::from(file), 8, test_ticket()).is_err());
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(super::WaylandShmPoolState::new(OwnedFd::from(socket), 4, test_ticket()).is_err());
}

#[test]
fn shm_truncate_after_pool_admission_never_dereferences_client_pages() {
    use std::os::fd::OwnedFd;
    let file = shm_file(&[1, 2, 3, 0, 4, 5, 6, 0]);
    let pool =
        super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 8, test_ticket())
            .unwrap();
    let buffer = shm_buffer(Arc::new(std::sync::Mutex::new(pool)), 0, 1, 2, 4);
    let (pixels, ..) = copy_pixels(&buffer).unwrap();
    assert_eq!(&**pixels, &[0xff03_0201, 0xff06_0504]);
    file.set_len(0).unwrap();
    assert!(copy_pixels(&buffer).is_none());
    assert_eq!(
        &**pixels,
        &[0xff03_0201, 0xff06_0504],
        "owned pixels survive truncate"
    );
}

#[test]
fn shm_failed_damage_read_is_transactional_and_preserves_shared_snapshot() {
    use std::os::fd::OwnedFd;
    let file = shm_file(&[1, 2, 3, 0, 4, 5, 6, 0]);
    let pool =
        super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 8, test_ticket())
            .unwrap();
    let buffer = shm_buffer(Arc::new(std::sync::Mutex::new(pool)), 0, 1, 2, 4);
    let retained: Arc<Vec<u32>> = Arc::new(vec![0xff11_1111, 0xff22_2222]);
    let presented = retained.clone();
    file.set_len(4).unwrap(); // first row is readable, second would SIGBUS under mmap
    let damage = Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 2,
    };
    // Planning succeeds; the read then fails short and produces no pixels, so
    // there is nothing to blit into the retained snapshot.
    let super::buffer::DamagePlan::Read(spec) = buffer.plan_damage(1, 2, 1, 2, damage) else {
        panic!("a layout-compatible damage commit plans a read");
    };
    assert!(spec.run().is_err());
    assert_eq!(retained, presented);
    assert!(
        Arc::ptr_eq(&retained, &presented),
        "failed read publishes no new pixels"
    );
}

#[test]
fn shm_pool_growth_validates_backing_before_publication_and_old_buffers_survive() {
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::FileExt;
    let file = shm_file(&[1, 2, 3, 0]);
    let pool = Arc::new(std::sync::Mutex::new(
        super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 4, test_ticket())
            .unwrap(),
    ));
    let old_buffer = shm_buffer(pool.clone(), 0, 1, 1, 4);
    assert!(pool.lock().unwrap().resize(8).is_err());
    assert_eq!(pool.lock().unwrap().len, 4);
    file.set_len(8).unwrap();
    file.write_all_at(&[4, 5, 6, 0], 4).unwrap();
    pool.lock().unwrap().resize(8).unwrap();
    assert!(pool.lock().unwrap().resize(4).is_err());
    assert_eq!(pool.lock().unwrap().len, 8);
    let new_buffer = shm_buffer(pool.clone(), 4, 1, 1, 4);
    assert_eq!(copy_pixels(&old_buffer).unwrap().0[0], 0xff03_0201);
    assert_eq!(copy_pixels(&new_buffer).unwrap().0[0], 0xff06_0504);
    file.set_len(4).unwrap();
    assert!(copy_pixels(&new_buffer).is_none());
}

#[test]
fn shm_pool_destroy_and_client_fd_close_do_not_retire_live_buffers() {
    use std::os::fd::OwnedFd;
    let file = shm_file(&[1, 2, 3, 0]);
    let data = super::ShmPoolData {
        shared: Arc::new(std::sync::Mutex::new(
            super::WaylandShmPoolState::new(
                OwnedFd::from(file.try_clone().unwrap()),
                4,
                test_ticket(),
            )
            .unwrap(),
        )),
    };
    let weak = Arc::downgrade(&data.shared);
    let buffer = shm_buffer(data.shared.clone(), 0, 1, 1, 4);
    drop(data); // wl_shm_pool.destroy
    drop(file); // client disconnect/close
    assert_eq!(copy_pixels(&buffer).unwrap().0[0], 0xff03_0201);
    let attached = buffer.clone();
    drop(buffer); // wl_buffer.destroy while attachment still owns backing
    assert!(weak.upgrade().is_some());
    assert_eq!(copy_pixels(&attached).unwrap().0[0], 0xff03_0201);
    drop(attached);
    assert!(
        weak.upgrade().is_none(),
        "last buffer/attachment retires pool fd"
    );
}

#[test]
fn shm_snapshots_remain_safe_while_another_thread_truncates_and_regrows() {
    use std::os::fd::AsRawFd;
    let file = shm_file(&vec![0x3a; 128 * 1024]);
    let writer = file.try_clone().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            for _ in 0..100 {
                writer.set_len(0).unwrap();
                writer.set_len(128 * 1024).unwrap();
            }
        });
        for _ in 0..100 {
            // A full owned snapshot or explicit short-I/O failure are both safe.
            if let Ok(bytes) = super::shm::read_shm_snapshot(file.as_raw_fd(), 0, 128 * 1024) {
                assert_eq!(bytes.len(), 128 * 1024);
            }
        }
    });
}

#[test]
fn shm_pool_admission_refuses_fds_whose_read_can_block_the_compositor() {
    use std::os::fd::AsRawFd;
    let file = shm_file(&vec![0x3a; 4096]);
    assert!(super::shm::validate_shm_extent(file.as_raw_fd(), 4096).is_ok());

    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: `ends` is a valid two-element out-array.
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
    for end in ends {
        let error = super::shm::validate_shm_extent(end, 4096).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        // SAFETY: both fds were created above and are closed exactly once.
        unsafe { libc::close(end) };
    }
}

#[test]
fn pointer_routing_requires_exact_ui_target_and_keeps_button_grab_on_original_client() {
    use wayland_server::{protocol::wl_surface::WlSurface, Resource};
    let display = test_display();
    let mut handle = display.handle();
    let mut peers = Vec::new();
    let mut state = super::WaylandState::new(1024, 768);
    for id in 1..=2 {
        let (server, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = handle
            .insert_client(server, Arc::new(super::WaylandClientState::default()))
            .unwrap();
        peers.push(peer);
        let surface = super::SurfaceData {
            shared: Arc::new(std::sync::Mutex::new(super::WaylandSurfaceState {
                surface_id: id,
                client_id: Some(client.id()),
                alive: true,
                frame: Rect {
                    x: 10,
                    y: 10,
                    width: 320,
                    height: 240,
                },
                width: 320,
                height: 240,
                ..Default::default()
            })),
        };
        let resource = client
            .create_resource::<WlSurface, _, super::WaylandState>(&handle, 6, surface.clone())
            .unwrap();
        surface.shared.lock().unwrap().resource = Some(resource);
        state.surfaces.push(surface);
    }
    // An occluding console/chrome/desktop is represented by no Wayland target.
    assert!(state.hit_test_pointer(100, 100).is_none());
    state.pointer_target = Some(1);
    let hit = state.hit_test_pointer(100, 100).unwrap();
    let first_resource = state.surfaces[0]
        .shared
        .lock()
        .unwrap()
        .resource
        .clone()
        .unwrap();
    assert_eq!(
        hit.surface.id(),
        first_resource.id(),
        "must not independently choose newer surface 2"
    );
    let first_client = hit.client_id.clone();
    state.update_pointer_focus(Some(hit));
    state.pointer_button_down = true;
    state.pointer_target = Some(2);
    let grabbed = state.hit_test_pointer(900, 700).unwrap();
    assert_eq!(grabbed.surface.id(), first_resource.id());
    assert_eq!(grabbed.client_id, first_client);
    assert!(grabbed.surface_x > 320.0 && grabbed.surface_y > 240.0);
    state.pointer_button_down = false;
    assert!(state.hit_test_pointer(900, 700).is_none());
    state.pointer_target = None;
    assert!(state.hit_test_pointer(100, 100).is_none());
}

#[test]
fn live_object_tickets_enforce_limits_and_release_on_drop() {
    use super::LiveObjectTicket;
    use std::sync::atomic::AtomicUsize;
    static TOTAL: AtomicUsize = AtomicUsize::new(0);
    let per_client = Arc::new(AtomicUsize::new(0));
    let a = LiveObjectTicket::admit(&per_client, 2, Some((&TOTAL, 3))).expect("first");
    let b = LiveObjectTicket::admit(&per_client, 2, Some((&TOTAL, 3))).expect("second");
    assert!(LiveObjectTicket::admit(&per_client, 2, Some((&TOTAL, 3))).is_none());
    assert_eq!(
        per_client.load(Ordering::Relaxed),
        2,
        "rejection must not leak a count"
    );
    // The global ceiling rejects a different client and rolls back its own count.
    let other = Arc::new(AtomicUsize::new(0));
    let c = LiveObjectTicket::admit(&other, 2, Some((&TOTAL, 3))).expect("third global");
    assert!(LiveObjectTicket::admit(&other, 2, Some((&TOTAL, 3))).is_none());
    assert_eq!(other.load(Ordering::Relaxed), 1);
    drop((a, b, c));
    assert_eq!(per_client.load(Ordering::Relaxed), 0);
    assert_eq!(other.load(Ordering::Relaxed), 0);
    assert_eq!(TOTAL.load(Ordering::Relaxed), 0);
}

/// Minimal raw-wire client: enough to prove the extension globals are
/// advertised and that their requests reach the dispatchers.
mod wire {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    pub fn message(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
        let size = (8 + args.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&object.to_ne_bytes());
        out.extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
        out.extend_from_slice(args);
        out
    }

    pub fn string(value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((value.len() + 1) as u32).to_ne_bytes());
        out.extend_from_slice(value.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    pub fn u32s(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    pub fn send(stream: &mut UnixStream, bytes: &[u8]) {
        stream.write_all(bytes).unwrap();
    }

    pub fn send_fd(stream: &UnixStream, bytes: &[u8], fd: std::os::fd::RawFd) {
        use std::os::fd::AsRawFd;
        // SAFETY: the iovec, control buffer and descriptor remain live throughout sendmsg.
        unsafe {
            let mut iov = libc::iovec {
                iov_base: bytes.as_ptr() as *mut _,
                iov_len: bytes.len(),
            };
            let mut control = [0usize; 8];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen =
                libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as usize;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as usize;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), fd);
            assert_eq!(
                libc::sendmsg(stream.as_raw_fd(), &msg, 0),
                bytes.len() as isize
            );
        }
    }

    /// Every event currently readable: `(object, opcode, payload)`.
    pub fn events(stream: &mut UnixStream) -> Vec<(u32, u16, Vec<u8>)> {
        stream.set_nonblocking(true).unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        let mut out = Vec::new();
        let mut at = 0;
        while at + 8 <= bytes.len() {
            let object = u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
            let word = u32::from_ne_bytes(bytes[at + 4..at + 8].try_into().unwrap());
            let (size, opcode) = ((word >> 16) as usize, (word & 0xffff) as u16);
            out.push((object, opcode, bytes[at + 8..at + size].to_vec()));
            at += size;
        }
        out
    }
}

fn pump(
    display: &mut wayland_server::Display<super::WaylandState>,
    state: &mut super::WaylandState,
) {
    display.dispatch_clients(state).unwrap();
    display.flush_clients().unwrap();
}

#[test]
fn chromium_extension_globals_are_advertised_and_dispatch() {
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    let (server, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(super::WaylandClientState::default()))
        .unwrap();

    // wl_display.get_registry(new_id = 2)
    wire::send(&mut peer, &wire::message(1, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    let mut names = std::collections::HashMap::new();
    for (object, opcode, payload) in wire::events(&mut peer) {
        if object == 2 && opcode == 0 {
            let name = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
            let len = u32::from_ne_bytes(payload[4..8].try_into().unwrap()) as usize;
            let interface = String::from_utf8(payload[8..8 + len - 1].to_vec()).unwrap();
            names.insert(interface, name);
        }
    }
    for interface in [
        "wp_viewporter",
        "wl_data_device_manager",
        "zwp_linux_dmabuf_v1",
        "zxdg_decoration_manager_v1",
    ] {
        assert!(
            names.contains_key(interface),
            "missing global {interface}: {names:?}"
        );
    }

    let bind = |interface: &str, version: u32, id: u32| {
        let mut args = wire::u32s(&[names[interface]]);
        args.extend(wire::string(interface));
        args.extend(wire::u32s(&[version, id]));
        wire::message(2, 0, &args)
    };

    // zwp_linux_dmabuf_v1 v3: bind, create_params, create(1x1, no planes) -> `failed`.
    wire::send(&mut peer, &bind("zwp_linux_dmabuf_v1", 3, 3));
    wire::send(&mut peer, &wire::message(3, 1, &wire::u32s(&[4])));
    wire::send(&mut peer, &wire::message(4, 2, &wire::u32s(&[1, 1, 0, 0])));
    pump(&mut display, &mut state);
    let events = wire::events(&mut peer);
    assert!(
        events
            .iter()
            .filter(|(object, opcode, _)| *object == 3 && *opcode == 0)
            .count()
            == 0
            && events
                .iter()
                .filter(|(object, opcode, _)| *object == 3 && *opcode == 1)
                .count()
                == 2,
        "only linear ARGB8888/XRGB8888 modifiers may be advertised: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|(object, opcode, _)| *object == 4 && *opcode == 1),
        "create must be answered with `failed`: {events:?}"
    );

    // wl_data_device_manager: an empty MIME type is a protocol error.
    wire::send(&mut peer, &bind("wl_data_device_manager", 3, 5));
    wire::send(&mut peer, &wire::message(5, 0, &wire::u32s(&[6])));
    wire::send(
        &mut peer,
        &wire::message(6, 0, &{
            let mut empty = wire::u32s(&[1]);
            empty.extend([0, 0, 0, 0]);
            empty
        }),
    );
    pump(&mut display, &mut state);
    assert!(
        wire::events(&mut peer)
            .iter()
            .any(|(object, opcode, _)| *object == 1 && *opcode == 0),
        "an empty MIME type must raise wl_display.error"
    );
}

#[test]
fn decoration_preference_is_configured_then_acknowledged_and_committed() {
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    let (server, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(super::WaylandClientState::default()))
        .unwrap();
    wire::send(&mut peer, &wire::message(1, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    let mut names = std::collections::HashMap::new();
    for (object, opcode, payload) in wire::events(&mut peer) {
        if object == 2 && opcode == 0 {
            let name = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
            let len = u32::from_ne_bytes(payload[4..8].try_into().unwrap()) as usize;
            names.insert(
                String::from_utf8(payload[8..8 + len - 1].to_vec()).unwrap(),
                name,
            );
        }
    }
    for (interface, version, id) in [
        ("wl_compositor", 6, 3),
        ("xdg_wm_base", 6, 4),
        ("zxdg_decoration_manager_v1", 1, 5),
    ] {
        let mut args = wire::u32s(&[names[interface]]);
        args.extend(wire::string(interface));
        args.extend(wire::u32s(&[version, id]));
        wire::send(&mut peer, &wire::message(2, 0, &args));
    }
    wire::send(&mut peer, &wire::message(3, 0, &wire::u32s(&[6])));
    wire::send(&mut peer, &wire::message(4, 2, &wire::u32s(&[7, 6])));
    wire::send(&mut peer, &wire::message(7, 1, &wire::u32s(&[8])));
    wire::send(&mut peer, &wire::message(5, 1, &wire::u32s(&[9, 8])));
    pump(&mut display, &mut state);
    wire::events(&mut peer);
    wire::send(&mut peer, &wire::message(9, 1, &wire::u32s(&[1])));
    pump(&mut display, &mut state);
    let events = wire::events(&mut peer);
    let deco = events
        .iter()
        .position(|(obj, op, p)| *obj == 9 && *op == 0 && p == &wire::u32s(&[1]))
        .unwrap();
    let config = events
        .iter()
        .position(|(obj, op, _)| *obj == 7 && *op == 0)
        .unwrap();
    assert!(
        deco < config,
        "decoration configure must precede xdg_surface.configure"
    );
    let serial = u32::from_ne_bytes(events[config].2[..4].try_into().unwrap());
    assert!(!state.surfaces[0].shared.lock().unwrap().decoration_server);
    wire::send(&mut peer, &wire::message(7, 4, &wire::u32s(&[serial])));
    wire::send(&mut peer, &wire::message(6, 6, &[]));
    pump(&mut display, &mut state);
    assert!(!state.surfaces[0].shared.lock().unwrap().decoration_server);
    wire::send(&mut peer, &wire::message(9, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    let events = wire::events(&mut peer);
    let deco = events
        .iter()
        .position(|(obj, op, p)| *obj == 9 && *op == 0 && p == &wire::u32s(&[2]))
        .unwrap();
    let config = events
        .iter()
        .position(|(obj, op, _)| *obj == 7 && *op == 0)
        .unwrap();
    assert!(deco < config);
    let serial = u32::from_ne_bytes(events[config].2[..4].try_into().unwrap());
    wire::send(&mut peer, &wire::message(7, 4, &wire::u32s(&[serial])));
    wire::send(&mut peer, &wire::message(6, 8, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    assert_eq!(
        state.surfaces[0]
            .shared
            .lock()
            .unwrap()
            .buffer_mapping
            .scale,
        1
    );
    assert!(!state.surfaces[0].shared.lock().unwrap().decoration_server);
    wire::send(&mut peer, &wire::message(6, 6, &[]));
    pump(&mut display, &mut state);
    assert_eq!(
        state.surfaces[0]
            .shared
            .lock()
            .unwrap()
            .buffer_mapping
            .scale,
        2
    );
    assert!(state.surfaces[0].shared.lock().unwrap().decoration_server);
    wire::send(&mut peer, &wire::message(9, 2, &[]));
    pump(&mut display, &mut state);
    let events = wire::events(&mut peer);
    assert!(events
        .iter()
        .any(|(obj, op, p)| *obj == 9 && *op == 0 && p == &wire::u32s(&[2])));
    let mut args = wire::u32s(&[names["wl_seat"]]);
    args.extend(wire::string("wl_seat"));
    args.extend(wire::u32s(&[7, 10]));
    wire::send(&mut peer, &wire::message(2, 0, &args));
    pump(&mut display, &mut state);
    let surface_id = {
        let mut surface = state.surfaces[0].shared.lock().unwrap();
        surface.frame = super::Rect {
            x: 100,
            y: 100,
            width: 40,
            height: 40,
        };
        surface.width = 40;
        surface.height = 40;
        surface.stride_pixels = 40;
        surface.pixels = Arc::new(vec![0xff00_0000; 1600]);
        surface.surface_id
    };
    state.pointer_target = Some(surface_id);
    state.pointer_x = 110;
    state.pointer_y = 138;
    assert!(state.pointer_button(1, true));
    let grab_serial = state.pointer_grab_serial.unwrap();
    wire::send(
        &mut peer,
        &wire::message(8, 5, &wire::u32s(&[10, grab_serial])),
    );
    pump(&mut display, &mut state);
    assert!(state.moving_surface.is_some());
    state.pointer_motion(210, 238);
    assert!(state.surfaces[0].shared.lock().unwrap().frame.x > 100);
    assert!(state.pointer_button(1, false));
    wire::send(&mut peer, &wire::message(5, 1, &wire::u32s(&[11, 8])));
    pump(&mut display, &mut state);
    assert!(
        wire::events(&mut peer)
            .iter()
            .any(|(obj, op, _)| *obj == 1 && *op == 0),
        "duplicate decoration is an error"
    );
}

#[test]
fn data_device_drag_cross_client_accept_drop_and_finish() {
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    state.display_handle = Some(display.handle());
    let (server_a, mut a) = std::os::unix::net::UnixStream::pair().unwrap();
    let (server_b, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
    for server in [server_a, server_b] {
        display
            .handle()
            .insert_client(server, Arc::new(super::WaylandClientState::default()))
            .unwrap();
    }
    for peer in [&mut a, &mut b] {
        wire::send(peer, &wire::message(1, 1, &wire::u32s(&[2])));
    }
    pump(&mut display, &mut state);
    for peer in [&mut a, &mut b] {
        let mut names = std::collections::HashMap::new();
        for (obj, op, p) in wire::events(peer) {
            if obj == 2 && op == 0 {
                let name = u32::from_ne_bytes(p[0..4].try_into().unwrap());
                let len = u32::from_ne_bytes(p[4..8].try_into().unwrap()) as usize;
                names.insert(String::from_utf8(p[8..8 + len - 1].to_vec()).unwrap(), name);
            }
        }
        for (iface, ver, id) in [
            ("wl_compositor", 6, 3),
            ("wl_seat", 7, 4),
            ("wl_data_device_manager", 3, 5),
        ] {
            let mut args = wire::u32s(&[names[iface]]);
            args.extend(wire::string(iface));
            args.extend(wire::u32s(&[ver, id]));
            wire::send(peer, &wire::message(2, 0, &args));
        }
        wire::send(peer, &wire::message(3, 0, &wire::u32s(&[6])));
        wire::send(peer, &wire::message(5, 1, &wire::u32s(&[7, 4])));
    }
    wire::send(&mut b, &wire::message(5, 1, &wire::u32s(&[8, 4])));
    pump(&mut display, &mut state);
    wire::events(&mut a);
    wire::events(&mut b);
    assert_eq!(state.surfaces.len(), 2);
    for (index, surface) in state.surfaces.iter().enumerate() {
        let mut surface = surface.shared.lock().unwrap();
        surface.frame = super::Rect {
            x: 10 + index * 100,
            y: 10,
            width: 50,
            height: 50,
        };
        surface.decoration_server = false;
        surface.width = 50;
        surface.height = 50;
        surface.stride_pixels = 50;
        surface.pixels = Arc::new(vec![0xff00_0000; 2500]);
    }
    let first_id = state.surfaces[0].shared.lock().unwrap().surface_id;
    let second_id = state.surfaces[1].shared.lock().unwrap().surface_id;
    state.pointer_x = 20;
    state.pointer_y = 20;
    state.pointer_target = Some(first_id);
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[8])));
    wire::send(
        &mut a,
        &wire::message(7, 1, &wire::u32s(&[8, serial.wrapping_add(100)])),
    );
    pump(&mut display, &mut state);
    assert!(
        state.selection.is_none(),
        "unrelated input serial must not set selection"
    );
    wire::send(&mut a, &wire::message(7, 1, &wire::u32s(&[8, serial])));
    pump(&mut display, &mut state);
    assert!(state.selection.is_some());
    wire::send(
        &mut a,
        &wire::message(7, 1, &wire::u32s(&[0, serial.wrapping_add(100)])),
    );
    pump(&mut display, &mut state);
    assert!(
        state.selection.is_some(),
        "unrelated input serial must not clear selection"
    );
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[9])));
    wire::send(&mut a, &wire::message(9, 0, &wire::string("text/plain")));
    wire::send(&mut a, &wire::message(9, 2, &wire::u32s(&[3])));
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[9, 6, 0, serial.wrapping_add(100)])),
    );
    pump(&mut display, &mut state);
    assert!(
        state.drag.is_none(),
        "an unrelated serial must not start a drag"
    );
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[9, 6, 0, serial])),
    );
    pump(&mut display, &mut state);
    state.pointer_target = Some(second_id);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    let first_enter = wire::events(&mut b);
    for device in [7, 8] {
        assert!(first_enter
            .iter()
            .any(|(obj, op, _)| *obj == device && *op == 1));
    }
    state.pointer_target = Some(first_id);
    state.pointer_motion(20, 20);
    display.flush_clients().unwrap();
    let left = wire::events(&mut b);
    for device in [7, 8] {
        assert!(left.iter().any(|(obj, op, _)| *obj == device && *op == 2));
    }
    state.pointer_target = Some(second_id);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    let events = wire::events(&mut b);
    let offer = events
        .iter()
        .find(|(obj, op, _)| *obj == 7 && *op == 0)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    let enter = events
        .iter()
        .find(|(obj, op, _)| *obj == 7 && *op == 1)
        .unwrap();
    let enter_serial = u32::from_ne_bytes(enter.2[..4].try_into().unwrap());
    let sibling = events
        .iter()
        .find(|(obj, op, _)| *obj == 8 && *op == 0)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    let sibling_serial = events
        .iter()
        .find(|(obj, op, _)| *obj == 8 && *op == 1)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    assert_ne!(offer, sibling);
    let mut sibling_accept = wire::u32s(&[sibling_serial]);
    sibling_accept.extend(wire::string("text/plain"));
    wire::send(&mut b, &wire::message(sibling, 0, &sibling_accept));
    wire::send(&mut b, &wire::message(sibling, 4, &wire::u32s(&[1, 1])));
    state.pointer_motion(121, 21);
    display.flush_clients().unwrap();
    let motion = wire::events(&mut b);
    for device in [7, 8] {
        assert!(motion.iter().any(|(obj, op, _)| *obj == device && *op == 3));
    }
    let mut accept = wire::u32s(&[enter_serial]);
    accept.extend(wire::string("text/plain"));
    wire::send(&mut b, &wire::message(offer, 0, &accept));
    wire::send(&mut b, &wire::message(offer, 4, &wire::u32s(&[1, 1])));
    pump(&mut display, &mut state);
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    let dropped = wire::events(&mut b);
    assert!(dropped.iter().any(|(obj, op, _)| *obj == 7 && *op == 4));
    assert!(!dropped.iter().any(|(obj, op, _)| *obj == 8 && *op == 4));
    for device in [7, 8] {
        assert!(dropped
            .iter()
            .any(|(obj, op, _)| *obj == device && *op == 2));
    }
    wire::events(&mut a);
    // A completed copy cannot be renegotiated to move after drop.
    wire::send(&mut b, &wire::message(offer, 4, &wire::u32s(&[3, 2])));
    pump(&mut display, &mut state);
    assert!(!wire::events(&mut a)
        .iter()
        .any(|(obj, op, _)| *obj == 9 && *op == 5));
    // Losing offers are invalid immediately, even before the winner finishes.
    let (read_end, write_end) = std::os::unix::net::UnixStream::pair().unwrap();
    use std::os::fd::AsRawFd;
    wire::send_fd(
        &b,
        &wire::message(sibling, 1, &wire::string("text/plain")),
        write_end.as_raw_fd(),
    );
    drop(write_end);
    pump(&mut display, &mut state);
    assert!(!wire::events(&mut a)
        .iter()
        .any(|(obj, op, _)| *obj == 9 && *op == 1));
    read_end.set_nonblocking(true).unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(std::io::Read::read(&mut &read_end, &mut byte).unwrap(), 0);
    wire::send(&mut b, &wire::message(offer, 3, &[]));
    pump(&mut display, &mut state);
    assert_eq!(
        wire::events(&mut a)
            .iter()
            .filter(|(obj, op, _)| *obj == 9 && *op == 4)
            .count(),
        1
    );

    // Releasing the winning device does not end its dropped offer; destroying
    // the losing offer has no effect, and destroying the winner cancels.
    state.pointer_target = Some(first_id);
    state.pointer_motion(20, 20);
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[10])));
    wire::send(&mut a, &wire::message(10, 0, &wire::string("text/plain")));
    wire::send(&mut a, &wire::message(10, 2, &wire::u32s(&[1])));
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[10, 6, 0, serial])),
    );
    pump(&mut display, &mut state);
    state.pointer_target = Some(second_id);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    let events = wire::events(&mut b);
    let mut cancel_offers = Vec::new();
    for device in [7, 8] {
        let id = events
            .iter()
            .find(|(obj, op, _)| *obj == device && *op == 0)
            .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
            .unwrap();
        let serial = events
            .iter()
            .find(|(obj, op, _)| *obj == device && *op == 1)
            .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
            .unwrap();
        let mut accept = wire::u32s(&[serial]);
        accept.extend(wire::string("text/plain"));
        wire::send(&mut b, &wire::message(id, 0, &accept));
        wire::send(&mut b, &wire::message(id, 4, &wire::u32s(&[1, 1])));
        cancel_offers.push(id);
    }
    pump(&mut display, &mut state);
    wire::events(&mut a);
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    let events = wire::events(&mut b);
    assert!(events.iter().any(|(obj, op, _)| *obj == 7 && *op == 4));
    assert!(!events.iter().any(|(obj, op, _)| *obj == 8 && *op == 4));
    for device in [7, 8] {
        assert!(events.iter().any(|(obj, op, _)| *obj == device && *op == 2));
    }
    wire::events(&mut a);
    // Releasing a device does not destroy its already-dropped offer.
    wire::send(&mut b, &wire::message(7, 2, &[]));
    pump(&mut display, &mut state);
    assert!(!wire::events(&mut a)
        .iter()
        .any(|(obj, op, _)| *obj == 10 && *op == 2));
    wire::send(&mut b, &wire::message(cancel_offers[1], 2, &[]));
    pump(&mut display, &mut state);
    assert!(!wire::events(&mut a)
        .iter()
        .any(|(obj, op, _)| *obj == 10 && *op == 2));
    wire::send(&mut b, &wire::message(cancel_offers[0], 2, &[]));
    pump(&mut display, &mut state);
    assert_eq!(
        wire::events(&mut a)
            .iter()
            .filter(|(obj, op, _)| *obj == 10 && *op == 2)
            .count(),
        1
    );

    // A null-source drag stays inside its originating client and still drops.
    wire::send(&mut a, &wire::message(3, 0, &wire::u32s(&[11])));
    pump(&mut display, &mut state);
    let icon_id = {
        let mut icon = state.surfaces.last().unwrap().shared.lock().unwrap();
        icon.width = 2;
        icon.height = 2;
        icon.stride_pixels = 2;
        icon.frame.width = 2;
        icon.frame.height = 2;
        icon.pixels = Arc::new(vec![1, 2, 3, 4]);
        icon.surface_id
    };
    state.pointer_target = Some(first_id);
    state.pointer_motion(20, 20);
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[0, 6, 11, serial])),
    );
    pump(&mut display, &mut state);
    assert!(state.drag.is_some());
    wire::send(
        &mut a,
        &wire::message(11, 10, &wire::u32s(&[(-21_i32) as u32, (-21_i32) as u32])),
    );
    wire::send(&mut a, &wire::message(11, 6, &[]));
    pump(&mut display, &mut state);
    let icon = state
        .window_snapshots()
        .into_iter()
        .find(|window| window.surface_id == icon_id)
        .unwrap();
    assert_eq!(
        (icon.frame.x, icon.frame.y, icon.width, icon.height),
        (0, 0, 1, 1)
    );
    assert_eq!(icon.pixels.as_ref(), &[4]);
    state.pointer_target = Some(second_id);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    assert!(!wire::events(&mut b)
        .iter()
        .any(|(obj, op, _)| *obj == 7 && *op == 1));
    state.pointer_target = Some(first_id);
    state.pointer_motion(20, 20);
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    assert!(wire::events(&mut a)
        .iter()
        .any(|(obj, op, _)| *obj == 7 && *op == 4));

    assert!(state
        .window_snapshots()
        .iter()
        .all(|window| window.surface_id != icon_id));

    // A v3 source is cancelled when the recipient accepts no offer.
    state.pointer_target = Some(first_id);
    state.pointer_motion(20, 20);
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[12])));
    wire::send(&mut a, &wire::message(12, 2, &wire::u32s(&[1])));
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[12, 6, 0, serial])),
    );
    pump(&mut display, &mut state);
    state.pointer_target = Some(second_id);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    assert!(wire::events(&mut b)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && *op == 1));
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    let rejected = wire::events(&mut b);
    assert!(rejected.iter().any(|(obj, op, _)| *obj == 8 && *op == 2));
    assert!(!rejected.iter().any(|(obj, op, _)| *obj == 8 && *op == 4));
    assert!(
        wire::events(&mut a)
            .iter()
            .any(|(obj, op, _)| *obj == 12 && *op == 2),
        "unaccepted v3 drag must be cancelled"
    );

    // Destroying the source while over a recipient ends the drag and leaves
    // every still-live recipient device.
    state.pointer_target = Some(first_id);
    state.pointer_motion(20, 20);
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[13])));
    wire::send(&mut a, &wire::message(13, 2, &wire::u32s(&[1])));
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[13, 6, 0, serial])),
    );
    pump(&mut display, &mut state);
    state.pointer_target = Some(second_id);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    assert!(wire::events(&mut b)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && *op == 1));
    wire::send(&mut a, &wire::message(13, 1, &[]));
    pump(&mut display, &mut state);
    assert!(state.drag.is_none());
    assert!(wire::events(&mut b)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && *op == 2));
    state.pointer_button(1, false);

    // Finishing a sibling after another offer won is a protocol error.
    wire::send(&mut b, &wire::message(sibling, 3, &[]));
    pump(&mut display, &mut state);
    assert!(wire::events(&mut b)
        .iter()
        .any(|(obj, op, _)| *obj == 1 && *op == 0));

    // A surface with an existing role cannot become a drag icon.
    state.pointer_target = Some(first_id);
    state.pointer_motion(20, 20);
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    state
        .surfaces
        .iter()
        .find(|surface| surface.shared.lock().unwrap().surface_id == first_id)
        .unwrap()
        .shared
        .lock()
        .unwrap()
        .role_assigned = true;
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[0, 6, 6, serial])),
    );
    pump(&mut display, &mut state);
    assert!(
        wire::events(&mut a)
            .iter()
            .any(|(obj, op, _)| *obj == 1 && *op == 0),
        "role conflict must be a protocol error"
    );
}

#[test]
fn viewport_only_commit_never_rereads_released_shm() {
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::FileExt;
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    let (server, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(super::WaylandClientState::default()))
        .unwrap();
    wire::send(&mut peer, &wire::message(1, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    let events = wire::events(&mut peer);
    let compositor = events
        .iter()
        .find_map(|(obj, op, p)| {
            if *obj != 2 || *op != 0 {
                return None;
            }
            let len = u32::from_ne_bytes(p[4..8].try_into().unwrap()) as usize;
            (&p[8..8 + len - 1] == b"wl_compositor")
                .then(|| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        })
        .unwrap();
    let mut args = wire::u32s(&[compositor]);
    args.extend(wire::string("wl_compositor"));
    args.extend(wire::u32s(&[6, 3]));
    wire::send(&mut peer, &wire::message(2, 0, &args));
    wire::send(&mut peer, &wire::message(3, 0, &wire::u32s(&[4])));
    pump(&mut display, &mut state);
    let file = shm_file(&[1, 2, 3, 0, 4, 5, 6, 0]);
    let pool =
        super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 8, test_ticket())
            .unwrap();
    let buffer = shm_buffer(Arc::new(std::sync::Mutex::new(pool)), 0, 2, 1, 8);
    let original = copy_pixels(&buffer).unwrap().0;
    {
        let mut surface = state.surfaces[0].shared.lock().unwrap();
        surface.current_buffer = Some(buffer);
        surface.original_pixels = original.clone();
        surface.original_width = 2;
        surface.original_height = 1;
        surface.pixels = original;
        surface.width = 2;
        surface.height = 1;
        surface.stride_pixels = 2;
        surface.pending_viewport.destination = Some((1, 1));
    }
    file.write_all_at(&[9; 8], 0).unwrap();
    wire::send(&mut peer, &wire::message(4, 6, &[]));
    pump(&mut display, &mut state);
    let surface = state.surfaces[0].shared.lock().unwrap();
    assert_eq!(surface.pixels.as_ref(), &[0xff06_0504]);
    assert_eq!(
        surface.original_pixels.as_ref(),
        &[0xff03_0201, 0xff06_0504]
    );
    assert!(
        !surface.commit.is_busy(),
        "viewport-only commit must not queue a client-buffer read"
    );
}

#[test]
fn data_device_modern_winner_excludes_legacy_and_incompatible_offers() {
    use std::os::fd::AsRawFd;
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    state.display_handle = Some(display.handle());
    let (server_a, mut a) = std::os::unix::net::UnixStream::pair().unwrap();
    let (server_b, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
    for server in [server_a, server_b] {
        display
            .handle()
            .insert_client(server, Arc::new(super::WaylandClientState::default()))
            .unwrap();
    }
    for peer in [&mut a, &mut b] {
        wire::send(peer, &wire::message(1, 1, &wire::u32s(&[2])));
    }
    pump(&mut display, &mut state);
    for (peer, manager_version) in [(&mut a, 3), (&mut b, 2)] {
        let mut names = std::collections::HashMap::new();
        for (obj, op, p) in wire::events(peer) {
            if obj == 2 && op == 0 {
                let name = u32::from_ne_bytes(p[0..4].try_into().unwrap());
                let len = u32::from_ne_bytes(p[4..8].try_into().unwrap()) as usize;
                names.insert(String::from_utf8(p[8..8 + len - 1].to_vec()).unwrap(), name);
            }
        }
        for (iface, ver, id) in [
            ("wl_compositor", 6, 3),
            ("wl_seat", 7, 4),
            ("wl_data_device_manager", manager_version, 5),
        ] {
            let mut args = wire::u32s(&[names[iface]]);
            args.extend(wire::string(iface));
            args.extend(wire::u32s(&[ver, id]));
            wire::send(peer, &wire::message(2, 0, &args));
        }
        wire::send(peer, &wire::message(3, 0, &wire::u32s(&[6])));
        wire::send(peer, &wire::message(5, 1, &wire::u32s(&[7, 4])));
        if manager_version == 2 {
            let mut args = wire::u32s(&[names["wl_data_device_manager"]]);
            args.extend(wire::string("wl_data_device_manager"));
            args.extend(wire::u32s(&[3, 8]));
            wire::send(peer, &wire::message(2, 0, &args));
            wire::send(peer, &wire::message(8, 1, &wire::u32s(&[9, 4])));
            wire::send(peer, &wire::message(5, 1, &wire::u32s(&[10, 4])));
            wire::send(peer, &wire::message(8, 1, &wire::u32s(&[11, 4])));
        }
    }
    pump(&mut display, &mut state);
    wire::events(&mut a);
    wire::events(&mut b);
    for (index, surface) in state.surfaces.iter().enumerate() {
        let mut surface = surface.shared.lock().unwrap();
        surface.frame = Rect {
            x: 10 + index * 100,
            y: 10,
            width: 50,
            height: 50,
        };
        surface.decoration_server = false;
        surface.width = 50;
        surface.height = 50;
        surface.stride_pixels = 50;
        surface.pixels = Arc::new(vec![0xff00_0000; 2500]);
    }
    let first = state.surfaces[0].shared.lock().unwrap().surface_id;
    let second = state.surfaces[1].shared.lock().unwrap().surface_id;
    state.pointer_target = Some(first);
    state.pointer_x = 20;
    state.pointer_y = 20;
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[8])));
    wire::send(&mut a, &wire::message(8, 0, &wire::string("text/plain")));
    wire::send(&mut a, &wire::message(8, 2, &wire::u32s(&[1])));
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[8, 6, 0, serial])),
    );
    pump(&mut display, &mut state);
    state.pointer_target = Some(second);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    let events = wire::events(&mut b);
    let offer = events
        .iter()
        .find(|(obj, op, _)| *obj == 7 && *op == 0)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    let find_offer = |device| {
        events
            .iter()
            .find(|(obj, op, _)| *obj == device && *op == 0)
            .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
            .unwrap()
    };
    let legacy2 = find_offer(10);
    let modern = find_offer(9);
    let incompatible = find_offer(11);
    let incompatible_serial = events
        .iter()
        .find(|(obj, op, _)| *obj == 11 && *op == 1)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    let mut rejected_accept = wire::u32s(&[incompatible_serial]);
    rejected_accept.extend(wire::string("text/plain"));
    wire::send(&mut b, &wire::message(incompatible, 0, &rejected_accept));
    // Move-only support cannot match the copy-only source.
    wire::send(
        &mut b,
        &wire::message(incompatible, 4, &wire::u32s(&[2, 2])),
    );
    let modern_serial = events
        .iter()
        .find(|(obj, op, _)| *obj == 9 && *op == 1)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    let mut accept = wire::u32s(&[modern_serial]);
    accept.extend(wire::string("text/plain"));
    wire::send(&mut b, &wire::message(modern, 0, &accept));
    // Source supports copy; destination may ordinarily prefer unsupported move.
    wire::send(&mut b, &wire::message(modern, 4, &wire::u32s(&[3, 2])));
    pump(&mut display, &mut state);
    let negotiation = wire::events(&mut b);
    assert!(negotiation
        .iter()
        .any(|(obj, op, p)| *obj == modern && *op == 2 && p == &wire::u32s(&[1])));
    assert!(!negotiation.iter().any(|(obj, op, _)| *obj == 1 && *op == 0));
    wire::events(&mut a);
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    let source_events = wire::events(&mut a);
    assert!(!source_events
        .iter()
        .any(|(obj, op, _)| *obj == 8 && *op == 4));
    let dropped = wire::events(&mut b);
    assert!(dropped.iter().any(|(obj, op, _)| *obj == 9 && *op == 4));
    for device in [7, 10, 11] {
        assert!(!dropped
            .iter()
            .any(|(obj, op, _)| *obj == device && *op == 4));
    }
    let (read_end, write_end) = std::os::unix::net::UnixStream::pair().unwrap();
    for legacy in [offer, legacy2] {
        wire::send_fd(
            &b,
            &wire::message(legacy, 1, &wire::string("text/plain")),
            write_end.as_raw_fd(),
        );
    }
    drop(write_end);
    pump(&mut display, &mut state);
    assert!(!wire::events(&mut a)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && *op == 1));
    read_end.set_nonblocking(true).unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(std::io::Read::read(&mut &read_end, &mut byte).unwrap(), 0);
    wire::send(&mut b, &wire::message(modern, 3, &[]));
    pump(&mut display, &mut state);
    assert_eq!(
        wire::events(&mut a)
            .iter()
            .filter(|(obj, op, _)| *obj == 8 && *op == 4)
            .count(),
        1
    );
}

fn wait_for_snapshot(state: &mut super::WaylandState) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while state.surfaces[0].shared.lock().unwrap().commit.is_busy() {
        assert!(std::time::Instant::now() < deadline, "snapshot timed out");
        state.pump_snapshots();
        std::thread::yield_now();
    }
}

#[test]
fn undamaged_replacement_uses_original_buffer_dimensions_for_viewport() {
    use std::os::fd::AsRawFd;
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    let (server, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(super::WaylandClientState::default()))
        .unwrap();
    wire::send(&mut peer, &wire::message(1, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    let mut names = std::collections::HashMap::new();
    for (obj, op, p) in wire::events(&mut peer) {
        if obj == 2 && op == 0 {
            let len = u32::from_ne_bytes(p[4..8].try_into().unwrap()) as usize;
            names.insert(
                String::from_utf8(p[8..8 + len - 1].to_vec()).unwrap(),
                u32::from_ne_bytes(p[..4].try_into().unwrap()),
            );
        }
    }
    for (iface, ver, id) in [
        ("wl_compositor", 6, 3),
        ("wp_viewporter", 1, 4),
        ("wl_shm", 1, 5),
    ] {
        let mut args = wire::u32s(&[names[iface]]);
        args.extend(wire::string(iface));
        args.extend(wire::u32s(&[ver, id]));
        wire::send(&mut peer, &wire::message(2, 0, &args));
    }
    wire::send(&mut peer, &wire::message(3, 0, &wire::u32s(&[6])));
    wire::send(&mut peer, &wire::message(4, 1, &wire::u32s(&[7, 6])));
    let mut bytes = vec![0x11; 8 * 8 * 4];
    bytes.extend(vec![0x22; 4 * 4 * 4]);
    let file = shm_file(&bytes);
    wire::send_fd(
        &peer,
        &wire::message(5, 0, &wire::u32s(&[8, bytes.len() as u32])),
        file.as_raw_fd(),
    );
    wire::send(
        &mut peer,
        &wire::message(8, 0, &wire::u32s(&[9, 0, 8, 8, 32, 1])),
    );
    wire::send(
        &mut peer,
        &wire::message(8, 0, &wire::u32s(&[10, 256, 4, 4, 16, 1])),
    );
    wire::send(&mut peer, &wire::message(7, 2, &wire::u32s(&[4, 4])));
    wire::send(&mut peer, &wire::message(6, 1, &wire::u32s(&[9, 0, 0])));
    wire::send(&mut peer, &wire::message(6, 6, &[]));
    pump(&mut display, &mut state);
    wait_for_snapshot(&mut state);
    state.flush_buffer_releases();
    display.flush_clients().unwrap();
    assert!(wire::events(&mut peer)
        .iter()
        .any(|(obj, op, _)| *obj == 9 && *op == 0));
    assert_eq!(state.surfaces[0].shared.lock().unwrap().original_width, 8);
    wire::send(&mut peer, &wire::message(6, 1, &wire::u32s(&[10, 0, 0])));
    wire::send(&mut peer, &wire::message(6, 6, &[]));
    pump(&mut display, &mut state);
    wait_for_snapshot(&mut state);
    state.flush_buffer_releases();
    display.flush_clients().unwrap();
    assert!(wire::events(&mut peer)
        .iter()
        .any(|(obj, op, _)| *obj == 10 && *op == 0));
    wire::send(&mut peer, &wire::message(7, 2, &wire::u32s(&[2, 2])));
    wire::send(&mut peer, &wire::message(6, 6, &[]));
    pump(&mut display, &mut state);
    wait_for_snapshot(&mut state);
    let surface = state.surfaces[0].shared.lock().unwrap();
    assert_eq!((surface.original_width, surface.original_height), (4, 4));
    assert_eq!(surface.original_pixels.len(), 16);
    assert_eq!((surface.width, surface.height), (2, 2));
    assert_eq!(surface.pixels.as_ref(), &[0xff22_2222; 4]);
}

#[test]
fn xdg_ack_survives_later_decoration_configure_and_duplicate_is_error() {
    use std::os::fd::AsRawFd;
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    let (server, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server, Arc::new(super::WaylandClientState::default()))
        .unwrap();
    wire::send(&mut peer, &wire::message(1, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    let mut names = std::collections::HashMap::new();
    for (obj, op, p) in wire::events(&mut peer) {
        if obj == 2 && op == 0 {
            let len = u32::from_ne_bytes(p[4..8].try_into().unwrap()) as usize;
            names.insert(
                String::from_utf8(p[8..8 + len - 1].to_vec()).unwrap(),
                u32::from_ne_bytes(p[..4].try_into().unwrap()),
            );
        }
    }
    for (iface, ver, id) in [
        ("wl_compositor", 6, 3),
        ("wl_shm", 1, 4),
        ("xdg_wm_base", 6, 5),
        ("zxdg_decoration_manager_v1", 1, 6),
    ] {
        let mut args = wire::u32s(&[names[iface]]);
        args.extend(wire::string(iface));
        args.extend(wire::u32s(&[ver, id]));
        wire::send(&mut peer, &wire::message(2, 0, &args));
    }
    wire::send(&mut peer, &wire::message(3, 0, &wire::u32s(&[7])));
    wire::send(&mut peer, &wire::message(5, 2, &wire::u32s(&[8, 7])));
    wire::send(&mut peer, &wire::message(8, 1, &wire::u32s(&[9])));
    wire::send(&mut peer, &wire::message(6, 1, &wire::u32s(&[10, 9])));
    pump(&mut display, &mut state);
    let events = wire::events(&mut peer);
    let serial = events
        .iter()
        .find(|(obj, op, _)| *obj == 8 && *op == 0)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    wire::send(&mut peer, &wire::message(8, 4, &wire::u32s(&[serial])));
    wire::send(&mut peer, &wire::message(10, 1, &wire::u32s(&[2])));
    pump(&mut display, &mut state);
    assert!(wire::events(&mut peer)
        .iter()
        .any(|(obj, op, _)| *obj == 8 && *op == 0));
    assert_eq!(
        state.surfaces[0].shared.lock().unwrap().acknowledged_serial,
        serial
    );
    let file = shm_file(&[0x33; 16]);
    wire::send_fd(
        &peer,
        &wire::message(4, 0, &wire::u32s(&[11, 16])),
        file.as_raw_fd(),
    );
    wire::send(
        &mut peer,
        &wire::message(11, 0, &wire::u32s(&[12, 0, 2, 2, 8, 1])),
    );
    wire::send(&mut peer, &wire::message(7, 1, &wire::u32s(&[12, 0, 0])));
    wire::send(&mut peer, &wire::message(7, 6, &[]));
    pump(&mut display, &mut state);
    wait_for_snapshot(&mut state);
    state.flush_buffer_releases();
    display.flush_clients().unwrap();
    let events = wire::events(&mut peer);
    assert!(!events.iter().any(|(obj, op, _)| *obj == 1 && *op == 0));
    assert!(events.iter().any(|(obj, op, _)| *obj == 12 && *op == 0));
    let surface = state.surfaces[0].shared.lock().unwrap();
    assert_eq!((surface.width, surface.height), (2, 2));
    assert!(!surface.decoration_server);
    drop(surface);
    wire::send(&mut peer, &wire::message(8, 4, &wire::u32s(&[serial])));
    pump(&mut display, &mut state);
    let error = wire::events(&mut peer)
        .into_iter()
        .find(|(obj, op, _)| *obj == 1 && *op == 0)
        .unwrap();
    assert_eq!(
        u32::from_ne_bytes(error.2[4..8].try_into().unwrap()),
        4,
        "xdg_surface.invalid_serial"
    );
}

#[test]
fn data_device_dropped_ask_rejects_preferred_action_outside_source_mask() {
    let mut display = test_display();
    super::create_wayland_globals(&display.handle());
    let mut state = super::WaylandState::new(1024, 768);
    state.display_handle = Some(display.handle());
    let (server_a, mut a) = std::os::unix::net::UnixStream::pair().unwrap();
    let (server_b, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
    for server in [server_a, server_b] {
        display
            .handle()
            .insert_client(server, Arc::new(super::WaylandClientState::default()))
            .unwrap();
    }
    for peer in [&mut a, &mut b] {
        wire::send(peer, &wire::message(1, 1, &wire::u32s(&[2])));
    }
    pump(&mut display, &mut state);
    for (peer, manager_version) in [(&mut a, 3), (&mut b, 2)] {
        let mut names = std::collections::HashMap::new();
        for (obj, op, p) in wire::events(peer) {
            if obj == 2 && op == 0 {
                let name = u32::from_ne_bytes(p[0..4].try_into().unwrap());
                let len = u32::from_ne_bytes(p[4..8].try_into().unwrap()) as usize;
                names.insert(String::from_utf8(p[8..8 + len - 1].to_vec()).unwrap(), name);
            }
        }
        for (iface, ver, id) in [
            ("wl_compositor", 6, 3),
            ("wl_seat", 7, 4),
            ("wl_data_device_manager", manager_version, 5),
        ] {
            let mut args = wire::u32s(&[names[iface]]);
            args.extend(wire::string(iface));
            args.extend(wire::u32s(&[ver, id]));
            wire::send(peer, &wire::message(2, 0, &args));
        }
        wire::send(peer, &wire::message(3, 0, &wire::u32s(&[6])));
        wire::send(peer, &wire::message(5, 1, &wire::u32s(&[7, 4])));
        if manager_version == 2 {
            let mut args = wire::u32s(&[names["wl_data_device_manager"]]);
            args.extend(wire::string("wl_data_device_manager"));
            args.extend(wire::u32s(&[3, 8]));
            wire::send(peer, &wire::message(2, 0, &args));
            wire::send(peer, &wire::message(8, 1, &wire::u32s(&[9, 4])));
            wire::send(peer, &wire::message(5, 1, &wire::u32s(&[10, 4])));
            wire::send(peer, &wire::message(8, 1, &wire::u32s(&[11, 4])));
        }
    }
    pump(&mut display, &mut state);
    wire::events(&mut a);
    wire::events(&mut b);
    for (index, surface) in state.surfaces.iter().enumerate() {
        let mut surface = surface.shared.lock().unwrap();
        surface.frame = Rect {
            x: 10 + index * 100,
            y: 10,
            width: 50,
            height: 50,
        };
        surface.decoration_server = false;
        surface.width = 50;
        surface.height = 50;
        surface.stride_pixels = 50;
        surface.pixels = Arc::new(vec![0xff00_0000; 2500]);
    }
    let first = state.surfaces[0].shared.lock().unwrap().surface_id;
    let second = state.surfaces[1].shared.lock().unwrap().surface_id;
    state.pointer_target = Some(first);
    state.pointer_x = 20;
    state.pointer_y = 20;
    assert!(state.pointer_button(1, true));
    let serial = state.pointer_grab_serial.unwrap();
    wire::send(&mut a, &wire::message(5, 0, &wire::u32s(&[8])));
    wire::send(&mut a, &wire::message(8, 0, &wire::string("text/plain")));
    wire::send(&mut a, &wire::message(8, 2, &wire::u32s(&[5])));
    wire::send(
        &mut a,
        &wire::message(7, 0, &wire::u32s(&[8, 6, 0, serial])),
    );
    pump(&mut display, &mut state);
    state.pointer_target = Some(second);
    state.pointer_motion(120, 20);
    display.flush_clients().unwrap();
    let events = wire::events(&mut b);
    let _offer = events
        .iter()
        .find(|(obj, op, _)| *obj == 7 && *op == 0)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    let find_offer = |device| {
        events
            .iter()
            .find(|(obj, op, _)| *obj == device && *op == 0)
            .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
            .unwrap()
    };
    let _legacy2 = find_offer(10);
    let modern = find_offer(9);
    let modern_serial = events
        .iter()
        .find(|(obj, op, _)| *obj == 9 && *op == 1)
        .map(|(_, _, p)| u32::from_ne_bytes(p[..4].try_into().unwrap()))
        .unwrap();
    let mut accept = wire::u32s(&[modern_serial]);
    accept.extend(wire::string("text/plain"));
    wire::send(&mut b, &wire::message(modern, 0, &accept));
    // The accepted ask action remains open until a final selection.
    wire::send(&mut b, &wire::message(modern, 4, &wire::u32s(&[5, 4])));
    pump(&mut display, &mut state);
    let negotiation = wire::events(&mut b);
    assert!(negotiation
        .iter()
        .any(|(obj, op, p)| *obj == modern && *op == 2 && p == &wire::u32s(&[4])));
    assert!(!negotiation.iter().any(|(obj, op, _)| *obj == 1 && *op == 0));
    wire::events(&mut a);
    assert!(state.pointer_button(1, false));
    display.flush_clients().unwrap();
    wire::events(&mut a);
    let dropped = wire::events(&mut b);
    assert!(dropped.iter().any(|(obj, op, _)| *obj == 9 && *op == 4));
    wire::send(&mut b, &wire::message(modern, 4, &wire::u32s(&[3, 2])));
    pump(&mut display, &mut state);
    let error = wire::events(&mut b)
        .into_iter()
        .find(|(obj, op, _)| *obj == 1 && *op == 0)
        .unwrap();
    assert_eq!(
        u32::from_ne_bytes(error.2[4..8].try_into().unwrap()),
        2,
        "wl_data_offer.invalid_action"
    );
}

mod arbitration;
mod dmabuf_import;
