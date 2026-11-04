use super::*;
use std::sync::Arc;

fn rect(x: usize, y: usize, width: usize, height: usize) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn window(outer: Rect, alpha: u32) -> WaylandWindowSnapshot {
    // The wire snapshot stores client dimensions; outer includes a 36px title
    // and one-pixel border on each side.
    let width = outer.width - 2;
    let height = outer.height - crate::layout::WINDOW_TITLE_HEIGHT - 2;
    WaylandWindowSnapshot {
        surface_id: 1,
        title: "fragments".into(),
        frame: rect(outer.x, outer.y, width, height),
        minimized: false,
        content_version: 1,
        damage: Rect::empty(),
        pixels: Arc::new(vec![(alpha << 24) | 0x123456; width * height]),
        width,
        height,
        stride_pixels: width,
    }
}

fn packed(
    layers: &[FragmentLayer],
    output: Rect,
    width: usize,
    height: usize,
) -> Result<(Vec<AtlasFragment>, Vec<Rect>), GpuSceneError> {
    let fragments = visible_fragments(layers, output)?;
    let mut pixels = vec![0; width * height];
    let mut packer = GpuAtlasPacker::new(&mut pixels, width, height, width)?;
    packer.reserve(CURSOR_TEXTURE_SIZE, CURSOR_TEXTURE_SIZE)?;
    reserve_fragments(&mut packer, fragments)
}

#[test]
fn desktop_and_2530x1390_maximized_window_fit_actual_2688x2560_atlas() {
    let output = rect(0, 0, 2560, 1440);
    let upper = window(rect(15, 25, 2530, 1390), 255);
    assert_eq!(wayland_window_outer_rect(&upper), rect(15, 25, 2530, 1390));
    let opaque = opaque_wayland_interior(&upper);
    let layers = [
        FragmentLayer {
            destination: output,
            opaque: output,
        },
        FragmentLayer {
            destination: wayland_window_outer_rect(&upper),
            opaque,
        },
        FragmentLayer {
            destination: taskbar_dirty_rect(2560, 1440).intersect(output),
            opaque: taskbar_dirty_rect(2560, 1440).intersect(output),
        },
    ];
    let (fragments, regions) =
        packed(&layers, output, 2688, 2560).expect("desktop + maximized client + dock + cursor");
    assert!(fragments.iter().any(|f| f.layer_index == 1));
    assert!(
        fragments
            .iter()
            .any(|f| f.layer_index == 0 && f.destination.contains(20, 30)),
        "desktop must remain beneath translucent title/corners"
    );
    assert!(fragments
        .iter()
        .filter(|f| f.layer_index == 0)
        .all(|f| f.destination.intersect(opaque).is_empty()));
    for (fragment, region) in fragments.iter().zip(&regions) {
        assert_eq!(
            (fragment.destination.width, fragment.destination.height),
            (region.width, region.height)
        );
    }
    let atlas = GpuTextureCapability {
        token: 7,
        generation: 1,
        content_epoch: 1,
        binding_slot: 0,
        width: 2688,
        height: 2560,
        stride_bytes: 2688 * 4,
    };
    let mut commands: Vec<_> = fragments
        .iter()
        .zip(&regions)
        .map(|(fragment, &source_rect)| GpuSceneLayer {
            destination: fragment.destination,
            opacity: 255,
            source_over: fragment.layer_index == 1,
            transform: GpuLayerTransform::flat(),
            kind: GpuLayerKind::Texture {
                region: GpuTextureRegion { atlas, source_rect },
            },
        })
        .collect();
    commands.push(GpuSceneLayer {
        destination: rect(0, 0, 64, 64),
        opacity: 255,
        source_over: true,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion {
                atlas,
                source_rect: rect(1, 1, 64, 64),
            },
        },
    });
    let mut compiler = crate::gpu_scene::GpuSceneCompiler::new(1, 1).unwrap();
    compiler.begin_prime(1).unwrap();
    compiler
        .complete_prime(driver_domain_protocol::DvmGpuPrimeCompletion {
            context_id: 1,
            context_epoch: 1,
            status: driver_domain_protocol::DvmGpuPrimeCompletionStatus::Ready,
            submit_flags: driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
            fence_value: 1,
            duration_ns: 1,
        })
        .unwrap();
    compiler.signal_acquire(1).unwrap();
    let batch = compiler
        .compile(2560, 1440, 1, 0xff000000, &commands)
        .unwrap();
    assert_eq!(
        batch.sources.len(),
        1,
        "one real ABI atlas, not one source per window"
    );
    batch.encode().expect("actual DVM wire validation");
}

#[test]
fn ordinary_overlapping_windows_preserve_every_visible_layer() {
    let output = rect(0, 0, 2560, 1440);
    let windows = [
        window(rect(15, 25, 2530, 1390), 255),
        window(rect(100, 110, 800, 520), 255),
        window(rect(500, 300, 800, 520), 255),
    ];
    let mut layers = vec![FragmentLayer {
        destination: output,
        opaque: output,
    }];
    layers.extend(windows.iter().map(|w| FragmentLayer {
        destination: wayland_window_outer_rect(w),
        opaque: opaque_wayland_interior(w),
    }));
    let dock = taskbar_dirty_rect(2560, 1440).intersect(output);
    layers.push(FragmentLayer {
        destination: dock,
        opaque: dock,
    });
    let (fragments, _) = packed(&layers, output, 2688, 2560)
        .expect("overlapping windows fit without dropping any window");
    for index in 0..layers.len() {
        assert!(fragments.iter().any(|f| f.layer_index == index));
    }
    for (index, layer) in layers.iter().enumerate() {
        for f in fragments.iter().filter(|f| f.layer_index == index) {
            assert_eq!(f.source.x + layer.destination.x, f.destination.x);
            assert_eq!(f.source.y + layer.destination.y, f.destination.y);
            for upper in &layers[index + 1..] {
                assert!(f.destination.intersect(upper.opaque).is_empty());
            }
        }
    }
}

#[test]
fn translucent_argb_never_proves_occlusion_and_titles_and_corners_survive() {
    let output = rect(0, 0, 320, 240);
    let upper = window(rect(10, 20, 200, 160), 127);
    assert!(opaque_wayland_interior(&upper).is_empty());
    let opaque_upper = window(rect(10, 20, 200, 160), 255);
    let interior = opaque_wayland_interior(&opaque_upper);
    let mut raster = render_wayland_gpu_texture(&opaque_upper, true).unwrap();
    for pixel in &mut raster {
        *pixel |= 0xff00_0000;
    }
    chrome::apply_window_texture_alpha(&mut raster, 200, 200, 160, true);
    for y in interior.y..interior.y + interior.height {
        for x in interior.x..interior.x + interior.width {
            assert_eq!(
                raster[(y - 20) * 200 + x - 10] >> 24,
                255,
                "every subtracted texel must be opaque"
            );
        }
    }
    assert!(
        raster[15 * 200 + 100] >> 24 < 255,
        "title glass blends with the lower layer"
    );
    assert!(
        raster[0] >> 24 < 255,
        "rounded corner must not hide desktop"
    );
    let layers = [
        FragmentLayer {
            destination: output,
            opaque: output,
        },
        FragmentLayer {
            destination: wayland_window_outer_rect(&upper),
            opaque: opaque_wayland_interior(&upper),
        },
    ];
    let fragments = visible_fragments(&layers, output).unwrap();
    assert_eq!(fragments[0].destination, output);
    assert_eq!(fragments[1].destination, layers[1].destination);
    // Pressure is explicit for a stack of large transparent surfaces, not a
    // false claim that the lower window/desktop has become invisible.
    let big_output = rect(0, 0, 2560, 1440);
    let transparent = window(rect(15, 25, 2530, 1390), 127);
    assert_eq!(
        packed(
            &[
                FragmentLayer {
                    destination: big_output,
                    opaque: big_output
                },
                FragmentLayer {
                    destination: wayland_window_outer_rect(&transparent),
                    opaque: opaque_wayland_interior(&transparent)
                },
            ],
            big_output,
            2688,
            2560
        )
        .unwrap_err(),
        GpuSceneError::AtlasFull
    );
}

#[test]
fn fragment_copy_uses_exact_source_coordinates_and_preserves_premultiplied_alpha() {
    let output = rect(0, 0, 12, 10);
    let layers = [
        FragmentLayer {
            destination: output,
            opaque: output,
        },
        FragmentLayer {
            destination: rect(3, 2, 7, 6),
            opaque: rect(5, 4, 3, 2),
        },
    ];
    let fragments = visible_fragments(&layers, output).unwrap();
    let mut pixels = vec![0xdeadbeef; 32 * 32];
    let mut packer = GpuAtlasPacker::new(&mut pixels, 32, 32, 32).unwrap();
    let (fragments, regions) = reserve_fragments(&mut packer, fragments).unwrap();
    let desktop: Vec<_> = (0..120).map(|n| n as u32).collect();
    let window: Vec<_> = (0..42).map(|n| 0x7f000000 | n).collect();
    write_fragment_layer(&mut packer, &fragments, &regions, 0, &desktop, 12, true).unwrap();
    write_fragment_layer(&mut packer, &fragments, &regions, 1, &window, 7, false).unwrap();
    let (pixels, stride) = packer.pixels_mut();
    for (f, region) in fragments.iter().zip(regions) {
        let (source, source_stride, alpha) = if f.layer_index == 0 {
            (&desktop, 12, 0xff000000)
        } else {
            (&window, 7, 0)
        };
        for y in 0..region.height {
            for x in 0..region.width {
                assert_eq!(
                    pixels[(region.y + y) * stride + region.x + x],
                    source[(f.source.y + y) * source_stride + f.source.x + x] | alpha
                );
            }
        }
        assert_eq!(
            pixels[(region.y - 1) * stride + region.x],
            0,
            "transparent gutter"
        );
    }
    // All pixels excluded from the lower layer are covered by proven opaque
    // texels above. The translucent remainder still has its lower source.
    for y in 0..10 {
        for x in 0..12 {
            let lower_count = fragments
                .iter()
                .filter(|f| f.layer_index == 0 && f.destination.contains(x, y))
                .count();
            assert_eq!(lower_count, usize::from(!layers[1].opaque.contains(x, y)));
        }
    }
}

#[test]
fn fragmentation_budget_rejects_whole_candidate_and_recovers_without_poisoning_layout() {
    let output = rect(0, 0, 2048, 100);
    let mut layers = vec![FragmentLayer {
        destination: output,
        opaque: output,
    }];
    // Disjoint, full-height occluders produce 511 desktop strips, more than
    // the 510-command budget even before their own visible commands.
    for x in (1..1021).step_by(2) {
        let destination = rect(x, 0, 1, 100);
        layers.push(FragmentLayer {
            destination,
            opaque: destination,
        });
    }
    assert_eq!(
        visible_fragments(&layers, output),
        Err(GpuSceneError::CommandLimit)
    );
    // Rejected scratch planning does not mutate a persistent allocator or
    // reserve commands from the next candidate.
    let recovered = packed(&layers[..2], output, 2200, 256).unwrap();
    assert_eq!(recovered.0.len(), 3);
}

#[test]
fn output_clipping_keeps_source_offset_in_the_independent_layer() {
    let output = rect(20, 30, 100, 80);
    let fragments = visible_fragments(
        &[FragmentLayer {
            destination: rect(10, 15, 200, 160),
            opaque: Rect::empty(),
        }],
        output,
    )
    .unwrap();
    assert_eq!(
        fragments,
        vec![AtlasFragment {
            layer_index: 0,
            destination: output,
            source: rect(10, 15, 100, 80),
        }]
    );
}

#[test]
fn fragmented_gpu_draw_order_matches_independent_layer_source_over_oracle() {
    let output = rect(0, 0, 80, 60);
    let layers = [
        FragmentLayer {
            destination: output,
            opaque: output,
        },
        FragmentLayer {
            destination: rect(5, 7, 68, 48),
            opaque: rect(9, 15, 60, 34),
        },
        FragmentLayer {
            destination: rect(20, 17, 50, 40),
            opaque: Rect::empty(),
        },
        FragmentLayer {
            destination: rect(40, 30, 50, 40),
            opaque: rect(44, 38, 42, 26),
        },
    ];
    let sources: Vec<Vec<u32>> = layers
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            (0..layer.destination.height)
                .flat_map(|y| {
                    (0..layer.destination.width).map(move |x| {
                        let alpha = if index == 0
                            || layer.opaque.contains(
                                (x + layer.destination.x) as u32,
                                (y + layer.destination.y) as u32,
                            ) {
                            255
                        } else {
                            127
                        };
                        (alpha << 24)
                            | (((x as u32 + index as u32 * 11) % 120) << 16)
                            | ((y as u32 % 120) << 8)
                            | index as u32
                    })
                })
                .collect()
        })
        .collect();
    let fragments = visible_fragments(&layers, output).unwrap();
    let mut atlas = vec![0; 128 * 128];
    let mut packer = GpuAtlasPacker::new(&mut atlas, 128, 128, 128).unwrap();
    let (fragments, regions) = reserve_fragments(&mut packer, fragments).unwrap();
    for (index, pixels) in sources.iter().enumerate() {
        write_fragment_layer(
            &mut packer,
            &fragments,
            &regions,
            index,
            pixels,
            layers[index].destination.width,
            false,
        )
        .unwrap();
    }
    let mut reference = vec![0xff000000; output.width * output.height];
    for (layer, pixels) in layers.iter().zip(&sources) {
        for y in 0..output.height {
            for x in 0..output.width {
                if layer.destination.contains(x as u32, y as u32) {
                    let source = pixels[(y - layer.destination.y) * layer.destination.width + x
                        - layer.destination.x];
                    let dest = &mut reference[y * output.width + x];
                    *dest = over(source, *dest);
                }
            }
        }
    }
    let mut actual = vec![0xff000000; output.width * output.height];
    for (fragment, region) in fragments.iter().zip(&regions) {
        for y in 0..region.height {
            for x in 0..region.width {
                let source = atlas[(region.y + y) * 128 + region.x + x];
                let dest = &mut actual
                    [(fragment.destination.y + y) * output.width + fragment.destination.x + x];
                *dest = over(source, *dest);
            }
        }
    }
    assert_eq!(
        actual, reference,
        "fragment selection must not change a single composited output pixel"
    );
}

fn over(source: u32, destination: u32) -> u32 {
    let inverse = 255 - (source >> 24);
    let mut result = 0;
    for shift in [0, 8, 16, 24] {
        let value =
            ((source >> shift) & 255) + (((destination >> shift) & 255) * inverse + 127) / 255;
        result |= value.min(255) << shift;
    }
    result
}

#[test]
fn only_capacity_failures_degrade_to_the_cropped_scene() {
    assert!(crate::render::degrades_to_cropped_scene(GpuSceneError::AtlasFull));
    assert!(crate::render::degrades_to_cropped_scene(
        GpuSceneError::CommandLimit
    ));
    assert!(!crate::render::degrades_to_cropped_scene(
        GpuSceneError::InvalidLayer
    ));
    assert!(!crate::render::degrades_to_cropped_scene(
        GpuSceneError::SceneNotReady
    ));
}
