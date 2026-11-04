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

fn test_display() -> wayland_server::Display<super::WaylandState> {
    wayland_server::Display::<super::WaylandState>::new().expect("display")
}

/// Client ids are only unique within one display, so tests that compare
/// clients must insert them into the same display.
fn test_client(
    display: &wayland_server::Display<super::WaylandState>,
) -> wayland_server::backend::ClientId {
    use std::os::unix::net::UnixStream;
    let (server_end, _client_end) = UnixStream::pair().expect("socket pair");
    display
        .handle()
        .insert_client(server_end, std::sync::Arc::new(super::WaylandClientState))
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
