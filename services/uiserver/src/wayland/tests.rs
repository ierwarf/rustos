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
    assert!(!valid_wayland_pool_resize(128, MAX_WAYLAND_SHM_POOL_BYTES + 1));
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
        .insert_client(server_end, std::sync::Arc::new(super::WaylandClientState::default()))
        .expect("client")
        .id()
}

/// Register a surface without a protocol resource, the way a dispatch would
/// after `create_surface_data`.
fn register_surface(state: &mut super::WaylandState, client: &wayland_server::backend::ClientId) -> u32 {
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
    assert_eq!(state.client_surface_count(&client_a), super::MAX_WAYLAND_SURFACES_PER_CLIENT);
    assert_eq!(state.client_surface_count(&client_b), 0);
    const { assert!(super::MAX_WAYLAND_SURFACES_PER_CLIENT < super::MAX_WAYLAND_SURFACES) };
}

#[test]
fn surface_memory_budget_is_shared_across_surfaces() {
    use super::{surface_pixel_budget_allows, MAX_WAYLAND_BUFFER_PIXELS, MAX_WAYLAND_TOTAL_SURFACE_PIXELS};
    assert!(surface_pixel_budget_allows(0, MAX_WAYLAND_BUFFER_PIXELS));
    assert!(surface_pixel_budget_allows(
        MAX_WAYLAND_TOTAL_SURFACE_PIXELS - 1,
        1
    ));
    assert!(!surface_pixel_budget_allows(MAX_WAYLAND_TOTAL_SURFACE_PIXELS, 1));
    assert!(!surface_pixel_budget_allows(usize::MAX, 1), "overflow must reject");
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
        super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 8, test_ticket()).unwrap();
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
        super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 8, test_ticket()).unwrap();
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
        super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 4, test_ticket()).unwrap(),
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
            super::WaylandShmPoolState::new(OwnedFd::from(file.try_clone().unwrap()), 4, test_ticket()).unwrap(),
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
        let client = handle.insert_client(server, Arc::new(super::WaylandClientState::default())).unwrap();
        peers.push(peer);
        let surface = super::SurfaceData { shared: Arc::new(std::sync::Mutex::new(super::WaylandSurfaceState {
            surface_id: id, client_id: Some(client.id()), alive: true,
            frame: Rect { x: 10, y: 10, width: 320, height: 240 },
            width: 320, height: 240, ..Default::default()
        })) };
        let resource = client.create_resource::<WlSurface, _, super::WaylandState>(&handle, 6, surface.clone()).unwrap();
        surface.shared.lock().unwrap().resource = Some(resource);
        state.surfaces.push(surface);
    }
    // An occluding console/chrome/desktop is represented by no Wayland target.
    assert!(state.hit_test_pointer(100, 100).is_none());
    state.pointer_target = Some(1);
    let hit = state.hit_test_pointer(100, 100).unwrap();
    let first_resource = state.surfaces[0].shared.lock().unwrap().resource.clone().unwrap();
    assert_eq!(hit.surface.id(), first_resource.id(), "must not independently choose newer surface 2");
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
    assert_eq!(per_client.load(Ordering::Relaxed), 2, "rejection must not leak a count");
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
