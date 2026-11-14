use std::sync::Arc;

use super::{
    copy_opaque_wayland_damage_to_atlas, push_console_gpu_layer, push_wayland_gpu_layer,
    reset_desktop_pixels, update_gpu_texture_layer_destination, wayland_window_client_rect,
    wayland_window_outer_rect, GpuAtlasPacker, GpuLayerKind, GpuLayerTransform, GpuSceneLayer,
    GpuTextureCapability, GpuTextureRegion, Rect, WaylandWindowSnapshot,
};

#[test]
fn shell_echo_updates_retained_atlas_without_dragging_the_window() {
    use crate::app::{ConsoleWindow, VisualUpdate};
    let frame = Rect {
        x: 40,
        y: 60,
        width: 320,
        height: 180,
    };
    let mut window = ConsoleWindow::new(7, String::from("Shell"), frame, 1);
    window.output_cache.extend_from_slice(b"prompt> ");
    super::chrome::rebuild_console_window_surface(&mut window, true);
    let mut atlas = window.surface_cache.pixels.clone();
    let before = atlas.clone();
    let source_rect = Rect {
        x: 0,
        y: 0,
        width: frame.width,
        height: frame.height,
    };
    let mut bindings = [super::ConsoleAtlasBinding {
        session_handle: 7,
        content_version: window.surface_cache.content_version,
        source_rect,
        layer_index: 0,
        focused: true,
    }];

    window.output_cache.push(b'a');
    window.terminal_dirty = true;
    window.invalidate_surface();
    let mut update = VisualUpdate::default();
    update.add_partial_rect(super::console_window_dirty_rect(frame));
    let mut damage = Vec::new();
    if update.scene_dirty() {
        assert!(super::update_console_gpu_texture(
            &mut window,
            true,
            &mut atlas,
            frame.width,
            &mut bindings,
            &mut 0,
            &mut damage,
        )
        .unwrap());
    }
    assert_eq!(window.frame, frame, "echo must not depend on window motion");
    assert_eq!(damage, vec![source_rect]);
    assert_ne!(atlas, before, "new glyph must reach the retained GPU atlas");
}

fn snapshot(pixels: Vec<u32>, damage: Rect) -> WaylandWindowSnapshot {
    WaylandWindowSnapshot {
        surface_id: 7,
        title: String::from("damage-test"),
        frame: Rect {
            x: 20,
            y: 30,
            width: 4,
            height: 3,
        },
        minimized: false,
        server_decoration: true,
        drag_icon: false,
        content_version: 2,
        damage,
        pixels: Arc::new(pixels),
        width: 4,
        height: 3,
        stride_pixels: 4,
    }
}

#[test]
fn opaque_wayland_update_copies_only_authenticated_damage() {
    let window = snapshot(
        (0..12).map(|index| 0xff00_0000 | index as u32).collect(),
        Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 1,
        },
    );
    let outer = wayland_window_outer_rect(&window);
    let source_rect = Rect {
        x: 8,
        y: 8,
        width: outer.width,
        height: outer.height,
    };
    let local_client = wayland_window_client_rect(Rect {
        x: 0,
        y: 0,
        width: outer.width,
        height: outer.height,
    });
    let stride = 128;
    let mut atlas = vec![0x1122_3344; stride * 128];
    let before = atlas.clone();
    let damage =
        copy_opaque_wayland_damage_to_atlas(atlas.as_mut_slice(), stride, source_rect, &window)
            .expect("valid damage")
            .expect("opaque damage uses the partial path");
    assert_eq!(
        damage,
        Rect {
            x: source_rect.x + local_client.x + 1,
            y: source_rect.y + local_client.y + 1,
            width: 2,
            height: 1,
        }
    );
    assert_eq!(atlas[damage.y * stride + damage.x], 0xff00_0005);
    assert_eq!(atlas[damage.y * stride + damage.x + 1], 0xff00_0006);
    for (index, (&new, &old)) in atlas.iter().zip(&before).enumerate() {
        let x = index % stride;
        let y = index / stride;
        if y == damage.y && (damage.x..damage.x + damage.width).contains(&x) {
            continue;
        }
        assert_eq!(new, old);
    }
}

#[test]
fn translucent_wayland_damage_falls_back_without_partial_mutation() {
    let mut pixels = vec![0xff00_0000; 12];
    pixels[5] = 0x7f00_0001;
    let window = snapshot(
        pixels,
        Rect {
            x: 1,
            y: 1,
            width: 1,
            height: 1,
        },
    );
    let outer = wayland_window_outer_rect(&window);
    let source_rect = Rect {
        x: 8,
        y: 8,
        width: outer.width,
        height: outer.height,
    };
    let mut atlas = vec![0x5566_7788; 128 * 128];
    let before = atlas.clone();
    assert_eq!(
        copy_opaque_wayland_damage_to_atlas(atlas.as_mut_slice(), 128, source_rect, &window,),
        Ok(None)
    );
    assert_eq!(atlas, before);
}

#[test]
fn retained_gpu_window_move_updates_only_command_destination() {
    let source_rect = Rect {
        x: 32,
        y: 48,
        width: 96,
        height: 64,
    };
    let capability = GpuTextureCapability {
        token: 7,
        generation: 11,
        content_epoch: 17,
        binding_slot: 0,
        width: 256,
        height: 256,
        stride_bytes: 1024,
    };
    let mut layers = [GpuSceneLayer {
        destination: Rect {
            x: 10,
            y: 20,
            width: 96,
            height: 64,
        },
        opacity: u8::MAX,
        source_over: false,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion {
                atlas: capability,
                source_rect,
            },
        },
    }];
    let moved = Rect {
        x: 700,
        y: 400,
        width: 96,
        height: 64,
    };

    assert!(update_gpu_texture_layer_destination(
        &mut layers,
        0,
        source_rect,
        moved
    ));
    assert_eq!(layers[0].destination, moved);
    assert_eq!(
        layers[0].kind,
        GpuLayerKind::Texture {
            region: GpuTextureRegion {
                atlas: capability,
                source_rect,
            }
        }
    );
    assert!(!update_gpu_texture_layer_destination(
        &mut layers,
        0,
        source_rect,
        Rect { width: 95, ..moved }
    ));
}

#[test]
fn a_window_that_cannot_fit_rejects_the_candidate_for_fragment_retry() {
    // Capacity is handled at the complete-scene boundary, not by silently
    // omitting this window.
    let mut pixels = vec![0_u32; 16 * 16];
    let mut packer = GpuAtlasPacker::new(pixels.as_mut_slice(), 16, 16, 16).expect("packer");
    let mut layers = Vec::new();
    let atlas = GpuTextureCapability {
        token: 1,
        generation: 1,
        content_epoch: 1,
        binding_slot: 0,
        width: 16,
        height: 16,
        stride_bytes: 64,
    };

    let window = snapshot(vec![0xff00_0000; 12], Rect::empty());
    let placed = push_wayland_gpu_layer(&mut packer, &mut layers, atlas, &window, false);
    assert_eq!(placed, Err(super::GpuSceneError::AtlasFull));
    assert!(layers.is_empty());
}

#[test]
fn the_desktop_composite_starts_as_a_full_copy_of_the_wallpaper() {
    // Pixels outside the chrome's own strips were never written, so the
    // gaps between the desktop and the dock stayed black.
    let background: Vec<u32> = (0..64 * 36).map(|index| 0x0010_2030 + index).collect();
    let mut pixels = vec![0_u32; 64 * 36];
    reset_desktop_pixels(&mut pixels, &background);
    assert_eq!(pixels, background);
    assert!(
        pixels.iter().all(|pixel| *pixel != 0),
        "no pixel is left black"
    );

    // A rebuild discards stale chrome rather than layering on top of it.
    pixels.iter_mut().for_each(|pixel| *pixel = 0x00ff_ffff);
    reset_desktop_pixels(&mut pixels, &background);
    assert_eq!(pixels, background);

    // A size change resizes the composite to match.
    let smaller = vec![0x0001_0203_u32; 16];
    reset_desktop_pixels(&mut pixels, &smaller);
    assert_eq!(pixels, smaller);
}

#[test]
fn a_console_that_cannot_fit_rejects_the_candidate_for_fragment_retry() {
    // The caller must retry the complete scene with visible fragments.
    use crate::app::ConsoleWindow;
    let mut pixels = vec![0_u32; 16 * 16];
    let mut packer = GpuAtlasPacker::new(pixels.as_mut_slice(), 16, 16, 16).expect("packer");
    let mut layers = Vec::new();
    let atlas = GpuTextureCapability {
        token: 1,
        generation: 1,
        content_epoch: 1,
        binding_slot: 0,
        width: 16,
        height: 16,
        stride_bytes: 64,
    };
    let frame = Rect {
        x: 40,
        y: 60,
        width: 320,
        height: 180,
    };
    let mut window = ConsoleWindow::new(7, String::from("Shell"), frame, 1);

    let placed = push_console_gpu_layer(&mut packer, &mut layers, atlas, &mut window, false);
    assert_eq!(placed, Err(super::GpuSceneError::AtlasFull));
    assert!(layers.is_empty());
}

#[test]
fn a_console_window_with_no_surface_is_left_out_rather_than_ending_the_scene() {
    // A zero-sized frame is reachable on a very small display.
    use crate::app::ConsoleWindow;
    let mut pixels = vec![0_u32; 512 * 512];
    let mut packer = GpuAtlasPacker::new(pixels.as_mut_slice(), 512, 512, 512).expect("packer");
    let mut layers = Vec::new();
    let atlas = GpuTextureCapability {
        token: 1,
        generation: 1,
        content_epoch: 1,
        binding_slot: 0,
        width: 512,
        height: 512,
        stride_bytes: 2048,
    };
    let mut window = ConsoleWindow::new(7, String::from("Shell"), Rect::empty(), 1);

    let placed = push_console_gpu_layer(&mut packer, &mut layers, atlas, &mut window, false)
        .expect("an empty window is skipped, not rejected");

    assert!(placed.is_none());
    assert!(layers.is_empty());
}

fn alpha_at(atlas: &[u32], stride: usize, x: usize, y: usize) -> u32 {
    atlas[y * stride + x] >> 24
}

#[test]
fn a_console_window_has_translucent_title_glass_and_an_opaque_body() {
    use crate::app::ConsoleWindow;
    let mut pixels = vec![0_u32; 512 * 512];
    let mut packer = GpuAtlasPacker::new(pixels.as_mut_slice(), 512, 512, 512).expect("packer");
    let mut layers = Vec::new();
    let atlas = GpuTextureCapability {
        token: 1,
        generation: 1,
        content_epoch: 1,
        binding_slot: 0,
        width: 512,
        height: 512,
        stride_bytes: 2048,
    };
    let frame = Rect {
        x: 40,
        y: 60,
        width: 320,
        height: 180,
    };
    let mut window = ConsoleWindow::new(7, String::from("Shell"), frame, 1);
    let region = push_console_gpu_layer(&mut packer, &mut layers, atlas, &mut window, true)
        .expect("fits")
        .expect("placed");
    assert!(layers[0].source_over, "translucent glass needs blending");
    drop(packer);

    let at = |x: usize, y: usize| alpha_at(&pixels, 512, region.x + x, region.y + y);
    let title = at(100, 12);
    assert!(
        title > 0 && title < 255,
        "title glass is translucent: {title}"
    );
    assert_eq!(at(100, 120), 255, "the body stays opaque");
    assert_eq!(at(0, 0), 0, "the rounded outer corner is transparent");
}

#[test]
fn a_wayland_window_has_translucent_title_glass_and_an_opaque_client() {
    let mut pixels = vec![0_u32; 64 * 64];
    let mut packer = GpuAtlasPacker::new(pixels.as_mut_slice(), 64, 64, 64).expect("packer");
    let mut layers = Vec::new();
    let atlas = GpuTextureCapability {
        token: 1,
        generation: 1,
        content_epoch: 1,
        binding_slot: 0,
        width: 64,
        height: 64,
        stride_bytes: 256,
    };
    let window = snapshot(vec![0xff00_0000; 12], Rect::empty());
    let region = push_wayland_gpu_layer(&mut packer, &mut layers, atlas, &window, true)
        .expect("fits")
        .expect("placed");
    assert!(layers[0].source_over);
    drop(packer);

    let at = |x: usize, y: usize| alpha_at(&pixels, 64, region.x + x, region.y + y);
    let title = at(3, 10);
    assert!(
        title > 0 && title < 255,
        "title glass is translucent: {title}"
    );
    assert_eq!(at(2, 38), 255, "the client area stays opaque");
}

#[test]
fn a_retained_console_update_keeps_the_title_glass_translucent() {
    use crate::app::ConsoleWindow;
    let frame = Rect {
        x: 40,
        y: 60,
        width: 320,
        height: 180,
    };
    let mut window = ConsoleWindow::new(7, String::from("Shell"), frame, 1);
    window.output_cache.extend_from_slice(b"prompt> ");
    super::chrome::rebuild_console_window_surface(&mut window, true);
    // The atlas holds the cache as an opaque copy before the update.
    let mut atlas: Vec<u32> = window
        .surface_cache
        .pixels
        .iter()
        .map(|pixel| pixel | 0xff00_0000)
        .collect();
    let source_rect = Rect {
        x: 0,
        y: 0,
        width: frame.width,
        height: frame.height,
    };
    let mut bindings = [super::ConsoleAtlasBinding {
        session_handle: 7,
        content_version: window.surface_cache.content_version,
        source_rect,
        layer_index: 0,
        focused: true,
    }];
    window.output_cache.push(b'x');
    window.terminal_dirty = true;
    window.invalidate_surface();
    let mut damage = Vec::new();
    assert!(super::update_console_gpu_texture(
        &mut window,
        true,
        &mut atlas,
        frame.width,
        &mut bindings,
        &mut 0,
        &mut damage,
    )
    .unwrap());

    let title = alpha_at(&atlas, frame.width, 100, 12);
    assert!(
        title > 0 && title < 255,
        "title glass is translucent: {title}"
    );
    assert_eq!(alpha_at(&atlas, frame.width, 100, 120), 255);
}

#[test]
fn a_console_window_that_fits_the_atlas_still_gets_its_layer() {
    use crate::app::ConsoleWindow;
    let mut pixels = vec![0_u32; 512 * 512];
    let mut packer = GpuAtlasPacker::new(pixels.as_mut_slice(), 512, 512, 512).expect("packer");
    let mut layers = Vec::new();
    let atlas = GpuTextureCapability {
        token: 1,
        generation: 1,
        content_epoch: 1,
        binding_slot: 0,
        width: 512,
        height: 512,
        stride_bytes: 2048,
    };
    let frame = Rect {
        x: 40,
        y: 60,
        width: 320,
        height: 180,
    };
    let mut window = ConsoleWindow::new(7, String::from("Shell"), frame, 1);

    let placed = push_console_gpu_layer(&mut packer, &mut layers, atlas, &mut window, true)
        .expect("fits")
        .expect("a window that fits gets a texture region");

    assert_eq!((placed.width, placed.height), (frame.width, frame.height));
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].destination, frame);
}

fn plan_snapshot(surface_id: u32, minimized: bool) -> WaylandWindowSnapshot {
    let mut window = snapshot(vec![0xff00_0000; 12], Rect::empty());
    window.surface_id = surface_id;
    window.minimized = minimized;
    window
}

#[test]
fn gpu_draw_plan_paints_the_focused_window_last_and_packs_it_first() {
    use super::{gpu_window_draw_plan, PlannedWindow};
    use crate::app::ConsoleWindow;
    use crate::app::WindowId::{Console, Wayland};
    let frame = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 64,
    };
    let consoles = [
        ConsoleWindow::new(1, String::from("a"), frame, 1),
        ConsoleWindow::new(2, String::from("b"), frame, 1),
    ];
    let wayland = [
        plan_snapshot(10, false),
        plan_snapshot(11, true),
        plan_snapshot(12, false),
    ];

    // A Wayland window has focus: unfocused Wayland, then consoles, then it.
    let plan = gpu_window_draw_plan(
        &wayland,
        &consoles,
        Some(12),
        0,
        &[
            Wayland(10),
            Console(1),
            Console(2),
            Wayland(11),
            Wayland(12),
        ],
    );
    assert_eq!(
        plan,
        vec![
            PlannedWindow::Wayland {
                index: 0,
                focused: false
            },
            PlannedWindow::Console {
                index: 0,
                focused: false
            },
            PlannedWindow::Console {
                index: 1,
                focused: false
            },
            PlannedWindow::Wayland {
                index: 2,
                focused: true
            },
        ]
    );
    // Packing runs backwards; a capacity failure retries the complete scene
    // as fragments rather than dropping a window.
    assert_eq!(
        plan.iter().rev().next(),
        Some(&PlannedWindow::Wayland {
            index: 2,
            focused: true
        })
    );

    // A console has focus and the focused Wayland id is gone: the console is
    // last, and a minimized or unknown focus never appears.
    let plan = gpu_window_draw_plan(
        &wayland,
        &consoles,
        None,
        2,
        &[Wayland(10), Console(1), Wayland(12), Console(2)],
    );
    assert_eq!(
        plan.last(),
        Some(&PlannedWindow::Console {
            index: 1,
            focused: true
        })
    );
    assert_eq!(
        plan.len(),
        4,
        "two Wayland, two consoles; the minimized one is skipped"
    );
    let plan = gpu_window_draw_plan(
        &wayland,
        &consoles,
        Some(11),
        1,
        &[
            Wayland(10),
            Console(1),
            Wayland(12),
            Console(2),
            Wayland(11),
        ],
    );
    assert!(plan
        .iter()
        .all(|window| !matches!(window, PlannedWindow::Wayland { index: 1, .. })));
    assert!(plan
        .iter()
        .all(|window| !matches!(window, PlannedWindow::Console { focused: true, .. })));
}

#[test]
fn mixed_draw_order_and_pointer_hits_agree_when_console_occludes_wayland() {
    use super::{gpu_window_draw_plan, PlannedWindow};
    use crate::app::{
        window_hit_test, ConsoleWindow,
        WindowId::{Console, Wayland},
    };
    use crate::layout::WindowChromeHit;
    let frame = Rect {
        x: 10,
        y: 10,
        width: 320,
        height: 240,
    };
    let mut consoles = [ConsoleWindow::new(1, "console".into(), frame, 1)];
    let mut wayland = [plan_snapshot(1, false)];
    wayland[0].frame = frame;
    wayland[0].width = frame.width;
    wayland[0].height = frame.height;
    let order = [Wayland(1), Console(1)];
    let hit = window_hit_test(&order, &consoles, &wayland, 100, 100);
    assert_eq!(hit, Some((Console(1), WindowChromeHit::Client)));
    let plan = gpu_window_draw_plan(&wayland, &consoles, None, 1, &order);
    assert_eq!(
        plan.last(),
        Some(&PlannedWindow::Console {
            index: 0,
            focused: true
        })
    );
    // Changing focus/order must expose exactly the window now painted last.
    let raised = [Console(1), Wayland(1)];
    assert_eq!(
        window_hit_test(&raised, &consoles, &wayland, 100, 100),
        Some((Wayland(1), WindowChromeHit::Client))
    );
    assert_eq!(
        gpu_window_draw_plan(&wayland, &consoles, Some(1), 0, &raised).last(),
        Some(&PlannedWindow::Wayland {
            index: 0,
            focused: true
        })
    );
    // Minimize preserves identity position but neither paints nor captures hits.
    consoles[0].minimized = true;
    assert_eq!(
        window_hit_test(&order, &consoles, &wayland, 100, 100),
        Some((Wayland(1), WindowChromeHit::Client))
    );
    assert_eq!(
        gpu_window_draw_plan(&wayland, &consoles, None, 0, &order).len(),
        1
    );
}

#[test]
fn upper_wayland_chrome_occludes_lower_client_area() {
    use crate::app::{window_hit_test, WindowId::Wayland};
    use crate::layout::WindowChromeHit;
    let mut lower = plan_snapshot(1, false);
    lower.frame = Rect {
        x: 10,
        y: 10,
        width: 320,
        height: 240,
    };
    lower.width = 320;
    lower.height = 240;
    let mut upper = plan_snapshot(2, false);
    upper.width = 320;
    upper.height = 240;
    upper.frame = Rect {
        x: 20,
        y: 100,
        width: 320,
        height: 240,
    };
    assert_eq!(
        window_hit_test(&[Wayland(1), Wayland(2)], &[], &[lower, upper], 80, 110),
        Some((Wayland(2), WindowChromeHit::TitleBar))
    );
}

#[test]
fn console_font_asset_revision_rebuilds_unchanged_title_and_terminal_raster() {
    use crate::app::ConsoleWindow;
    let frame = Rect {
        x: 40,
        y: 60,
        width: 320,
        height: 180,
    };
    let mut window = ConsoleWindow::new(7, String::from("Shell"), frame, 1);
    window.output_cache.extend_from_slice(b"unchanged terminal");
    super::chrome::rebuild_console_window_surface(&mut window, true);
    let previous_version = window.surface_cache.content_version;
    assert!(!window.terminal_dirty);
    window.surface_cache.font_asset_revision = crate::font::asset_revision().wrapping_sub(1);
    super::chrome::rebuild_console_window_surface(&mut window, true);
    assert_ne!(window.surface_cache.content_version, previous_version);
    assert_eq!(
        window.surface_cache.font_asset_revision,
        crate::font::asset_revision()
    );
    let current_version = window.surface_cache.content_version;
    super::chrome::rebuild_console_window_surface(&mut window, true);
    assert_eq!(
        window.surface_cache.content_version, current_version,
        "same font revision reuses the raster"
    );
}

#[test]
fn client_decorated_wayland_window_has_no_compositor_chrome() {
    let mut window = snapshot(vec![0xff12_3456; 4], Rect::empty());
    window.width = 2;
    window.height = 2;
    window.stride_pixels = 2;
    window.frame.width = 2;
    window.frame.height = 2;
    window.server_decoration = false;
    assert_eq!(wayland_window_outer_rect(&window), window.frame);
    let pixels = super::render_wayland_gpu_texture(&window, true).unwrap();
    assert_eq!(pixels, vec![0xff12_3456; 4]);
    let hit = crate::app::window_hit_test(
        &[crate::app::WindowId::Wayland(window.surface_id)],
        &[],
        &[window.clone()],
        window.frame.x as u32,
        window.frame.y as u32,
    );
    assert!(matches!(
        hit,
        Some((_, crate::layout::WindowChromeHit::Client))
    ));
}
