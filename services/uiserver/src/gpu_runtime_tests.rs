// SPDX-License-Identifier: MIT

use super::{
    AtlasDamageEpoch, DvmGpuAtlasDamage, GPU_COMPLETION_TIMEOUT, GPU_FIRST_FRAME_TIMEOUT,
    GPU_FRAME_INTERVAL, GPU_INITIALIZATION_RETAINS_BOOT_CLASS, GPU_PROVIDER_HEALTH_INTERVAL,
    GpuCompositorRuntime, GpuProviderAdmission, Rect, completion_wait_origin,
    copy_atlas_damage_to_slot, difference_bounds, difference_damage_into, gpu_completion_timed_out,
    gpu_provider_admission, gpu_ready_retry_backoff, next_frame_deadline, reconstruction_damage_within_budget,
    snapshot_damage_for_slot,
};
use crate::sys::DisplayInfo;
use rustos_user_abi::device::{DISPLAY_INFO_FLAG_DVM_SCANOUT, DISPLAY_INFO_FLAG_GPU_COMPOSITOR};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

fn display_with_flags(flags: u32) -> DisplayInfo {
    DisplayInfo {
        width: 1600,
        height: 900,
        stride_bytes: 7168,
        bytes_per_pixel: 4,
        pixel_format: 1,
        flags,
        generation: 1,
    }
}

#[test]
fn gpu_provider_retry_backoff_is_bounded_and_exponential() {
    assert_eq!(gpu_ready_retry_backoff(1), Duration::from_millis(50));
    assert_eq!(gpu_ready_retry_backoff(2), Duration::from_millis(100));
    assert_eq!(gpu_ready_retry_backoff(3), Duration::from_millis(200));
    assert_eq!(gpu_ready_retry_backoff(4), Duration::from_millis(400));
    assert_eq!(
        gpu_ready_retry_backoff(u32::MAX),
        Duration::from_millis(400)
    );
}

#[test]
fn dvm_gpu_admission_waits_without_hiding_behind_software() {
    assert_eq!(
        gpu_provider_admission(display_with_flags(0)),
        GpuProviderAdmission::SoftwareFallback
    );
    assert_eq!(
        gpu_provider_admission(display_with_flags(DISPLAY_INFO_FLAG_DVM_SCANOUT)),
        GpuProviderAdmission::WaitForDvmGpu
    );
    assert_eq!(
        gpu_provider_admission(display_with_flags(
            DISPLAY_INFO_FLAG_DVM_SCANOUT | DISPLAY_INFO_FLAG_GPU_COMPOSITOR,
        )),
        GpuProviderAdmission::Ready
    );
    assert_eq!(
        gpu_provider_admission(display_with_flags(DISPLAY_INFO_FLAG_GPU_COMPOSITOR)),
        GpuProviderAdmission::Invalid
    );
}

#[test]
fn mandatory_gpu_wait_never_admits_cpu_present_as_retry() {
    assert!(GpuCompositorRuntime::SoftwareFallback.admits_cpu_present());
    let waiting = GpuCompositorRuntime::Waiting {
        deadline: Instant::now() + Duration::from_secs(1),
        next_probe: Instant::now(),
        initialization: None,
    };
    assert!(!waiting.admits_cpu_present());
}

#[test]
fn mandatory_gpu_initialization_retains_boot_critical_class_until_result() {
    assert!(GPU_INITIALIZATION_RETAINS_BOOT_CLASS);
}

#[test]
fn idle_provider_revoke_has_a_bounded_low_rate_probe() {
    assert!(GPU_PROVIDER_HEALTH_INTERVAL <= Duration::from_millis(100));
    assert!(GPU_PROVIDER_HEALTH_INTERVAL >= Duration::from_millis(10));
    assert_eq!(
        gpu_provider_admission(display_with_flags(DISPLAY_INFO_FLAG_DVM_SCANOUT)),
        GpuProviderAdmission::WaitForDvmGpu
    );
}

#[test]
fn difference_bounds_is_empty_for_identical_atlases() {
    let atlas = vec![0_u32; 4 * 3];
    assert_eq!(difference_bounds(&atlas, &atlas, 4, 3, 4), None);
}

#[test]
fn difference_bounds_covers_only_the_changed_rectangle() {
    let previous = vec![0_u32; 6 * 4];
    let mut next = previous.clone();
    next[1 + 6] = 1;
    next[4 + 3 * 6] = 2;
    assert_eq!(
        difference_bounds(&previous, &next, 5, 4, 6),
        Some(Rect {
            x: 1,
            y: 1,
            width: 4,
            height: 3,
        })
    );
}

#[test]
fn difference_bounds_rejects_incompatible_geometry() {
    assert_eq!(difference_bounds(&[0; 4], &[0; 5], 2, 2, 2), None);
    assert_eq!(difference_bounds(&[0; 4], &[0; 4], 3, 2, 2), None);
}

#[test]
fn difference_damage_keeps_distant_bands_and_merges_only_identical_adjacent_spans() {
    let (width, height, stride) = (12, 12, 14);
    let previous = vec![0; stride * height];
    let mut next = previous.clone();
    next[stride + 2..stride + 5].fill(1);
    next[2 * stride + 2..2 * stride + 5].fill(2);
    next[3 * stride + 3..3 * stride + 6].fill(3);
    next[8 * stride + 9] = 4;
    let mut damage = Vec::new();
    difference_damage_into(&previous, &next, width, height, stride, &mut damage).unwrap();
    assert_eq!(damage, vec![
        DvmGpuAtlasDamage { x: 2, y: 1, width: 3, height: 2 },
        DvmGpuAtlasDamage { x: 3, y: 3, width: 3, height: 1 },
        DvmGpuAtlasDamage { x: 9, y: 8, width: 1, height: 1 },
    ]);
    assert!(driver_domain_protocol::dvm_gpu_atlas_damage_is_valid(
        &damage, width as u32, height as u32, false,
    ));
}

#[test]
fn difference_damage_collapses_at_the_wire_cap_and_keeps_later_changes() {
    let limit = driver_domain_protocol::DVM_GPU_ATLAS_MAX_DAMAGE_RECTS as usize;
    let (width, height, stride) = (3, limit * 2 + 3, 5);
    let previous = vec![0; stride * height];
    let mut next = previous.clone();
    for band in 0..limit {
        next[band * 2 * stride + band % width] = 1;
    }
    let mut damage = Vec::new();
    difference_damage_into(&previous, &next, width, height, stride, &mut damage).unwrap();
    assert_eq!(damage.len(), limit, "the exact wire cap is still representable");
    let capacity = damage.capacity();
    next[limit * 2 * stride + 2] = 2;
    next[(height - 1) * stride] = 3;
    difference_damage_into(&previous, &next, width, height, stride, &mut damage).unwrap();
    assert_eq!(damage, vec![DvmGpuAtlasDamage {
        x: 0,
        y: 0,
        width: width as u32,
        height: height as u32,
    }]);
    assert_eq!(damage.capacity(), capacity, "the reusable buffer is retained");
    let mut reconstructed = previous;
    copy_atlas_damage_to_slot(
        &mut reconstructed, &next, stride, &damage, width as u32, height as u32,
    ).unwrap();
    assert_eq!(reconstructed, next);
}

#[test]
fn difference_damage_ignores_stride_padding_and_handles_a_short_final_row() {
    let (width, height, stride) = (5, 4, 8);
    let previous = vec![0; 2 * stride + 3];
    let mut next = previous.clone();
    next[6] = 99; // padding, never sampled by the atlas image
    next[1] = 1;
    next[stride + 4] = 2;
    next[2 * stride + 2] = 3; // final present texel in the truncated third row
    let mut damage = Vec::new();
    difference_damage_into(&previous, &next, width, height, stride, &mut damage).unwrap();
    assert_eq!(damage, vec![
        DvmGpuAtlasDamage { x: 1, y: 0, width: 1, height: 1 },
        DvmGpuAtlasDamage { x: 4, y: 1, width: 1, height: 1 },
        DvmGpuAtlasDamage { x: 2, y: 2, width: 1, height: 1 },
    ]);
    let capacity = damage.capacity();
    difference_damage_into(&next, &next, width, height, stride, &mut damage).unwrap();
    assert!(damage.is_empty(), "a reused output cannot retain old damage");
    assert_eq!(damage.capacity(), capacity);
    assert!(difference_damage_into(
        &previous, &next[..next.len() - 1], width, height, stride, &mut damage,
    ).is_err());
    assert!(damage.is_empty());
}

#[test]
fn frame_deadline_is_spacing_from_submission_not_a_phase_grid() {
    let origin = Instant::now();
    assert_eq!(next_frame_deadline(origin), origin + GPU_FRAME_INTERVAL);
    // An update after a long idle gap is never delayed by a stale grid slot.
    let later = origin + Duration::from_millis(49);
    assert!(next_frame_deadline(origin) < later);
}

#[test]
fn completion_timeout_separates_activation_from_steady_state() {
    let submitted_at = Instant::now();
    let activation_deadline = submitted_at + GPU_FIRST_FRAME_TIMEOUT;
    assert!(!gpu_completion_timed_out(
        submitted_at,
        activation_deadline,
        true,
        submitted_at + GPU_COMPLETION_TIMEOUT - Duration::from_nanos(1),
    ));
    assert!(gpu_completion_timed_out(
        submitted_at,
        activation_deadline,
        true,
        submitted_at + GPU_COMPLETION_TIMEOUT,
    ));
    assert!(!gpu_completion_timed_out(
        submitted_at,
        activation_deadline,
        false,
        activation_deadline - Duration::from_nanos(1),
    ));
    assert!(gpu_completion_timed_out(
        submitted_at,
        activation_deadline,
        false,
        activation_deadline,
    ));
}

#[test]
fn snapshot_damage_keeps_partial_patch_for_exact_slot_predecessor() {
    let requested = [DvmGpuAtlasDamage {
        x: 7,
        y: 11,
        width: 13,
        height: 17,
    }];
    assert_eq!(
        snapshot_damage_for_slot(8, 9, &requested, &VecDeque::new(), 1600, 900),
        Ok(requested.to_vec())
    );
}

#[test]
fn exact_predecessor_damage_is_normalized_for_the_wire_contract() {
    let requested = [
        DvmGpuAtlasDamage { x: 2, y: 3, width: 8, height: 9 },
        DvmGpuAtlasDamage { x: 5, y: 7, width: 8, height: 9 },
        DvmGpuAtlasDamage { x: 50, y: 60, width: 1, height: 1 },
    ];
    let damage = snapshot_damage_for_slot(8, 9, &requested, &VecDeque::new(), 128, 128)
        .unwrap();
    assert!(driver_domain_protocol::dvm_gpu_atlas_damage_is_valid(
        &damage, 128, 128, false,
    ));
    assert_eq!(damage.len(), 2);
    let source: Vec<u32> = (0..128 * 128).collect();
    let mut destination = vec![u32::MAX; source.len()];
    copy_atlas_damage_to_slot(&mut destination, &source, 128, &damage, 128, 128).unwrap();
    for rect in requested {
        for y in rect.y..rect.y + rect.height {
            let start = (y * 128 + rect.x) as usize;
            let end = start + rect.width as usize;
            assert_eq!(&destination[start..end], &source[start..end]);
        }
    }

    let fragmented: Vec<_> = (0..65).map(|index| DvmGpuAtlasDamage {
        x: index * 2,
        y: 0,
        width: 1,
        height: 1,
    }).collect();
    let damage = snapshot_damage_for_slot(8, 9, &fragmented, &VecDeque::new(), 256, 4)
        .unwrap();
    assert_eq!(damage.len(), 1);
    assert!(damage[0].is_full_atlas(256, 4));
}

#[test]
fn rejected_overlap_never_partially_writes_a_slot() {
    let source = vec![1; 256];
    let mut destination = vec![0; 256];
    let damage = [
        DvmGpuAtlasDamage { x: 0, y: 0, width: 16, height: 8 },
        DvmGpuAtlasDamage { x: 0, y: 4, width: 16, height: 8 },
    ];
    assert_eq!(
        copy_atlas_damage_to_slot(&mut destination, &source, 16, &damage, 16, 16),
        Err(super::EINVAL),
    );
    assert!(destination.iter().all(|&pixel| pixel == 0));
}

#[test]
fn slot_mapping_copy_changes_only_validated_damage() {
    let source = (0_u32..32).collect::<Vec<_>>();
    let mut destination = vec![u32::MAX; 32];
    let damage = [DvmGpuAtlasDamage {
        x: 2,
        y: 1,
        width: 3,
        height: 2,
    }];
    copy_atlas_damage_to_slot(&mut destination, &source, 8, &damage, 8, 4)
        .expect("copy bounded atlas damage");
    for index in 0..32 {
        let x = index % 8;
        let y = index / 8;
        if (2..5).contains(&x) && (1..3).contains(&y) {
            assert_eq!(destination[index], source[index]);
        } else {
            assert_eq!(destination[index], u32::MAX);
        }
    }
}

#[test]
fn slot_mapping_streams_large_unaligned_rows_exactly() {
    let source = (0_u32..257).collect::<Vec<_>>();
    let mut allocation = vec![u32::MAX; 258];
    let destination = &mut allocation[1..258];
    let damage = [DvmGpuAtlasDamage {
        x: 1,
        y: 0,
        width: 255,
        height: 1,
    }];
    copy_atlas_damage_to_slot(destination, &source, 257, &damage, 257, 1)
        .expect("stream bounded atlas damage");
    assert_eq!(destination[0], u32::MAX);
    assert_eq!(&destination[1..256], &source[1..256]);
    assert_eq!(destination[256], u32::MAX);
}

#[test]
fn snapshot_damage_forces_full_copy_for_uninitialized_or_stale_slot() {
    let requested = [DvmGpuAtlasDamage {
        x: 7,
        y: 11,
        width: 13,
        height: 17,
    }];
    let full = vec![DvmGpuAtlasDamage {
        x: 0,
        y: 0,
        width: 1600,
        height: 900,
    }];
    assert_eq!(
        snapshot_damage_for_slot(0, 1, &requested, &VecDeque::new(), 1600, 900),
        Ok(full.clone())
    );
    assert_eq!(
        snapshot_damage_for_slot(6, 9, &requested, &VecDeque::new(), 1600, 900),
        Ok(full)
    );
}

#[test]
fn snapshot_damage_replays_bounded_history_for_rotated_slot() {
    let history = VecDeque::from([
        AtlasDamageEpoch {
            epoch: 7,
            damage: vec![DvmGpuAtlasDamage {
                x: 10,
                y: 20,
                width: 4,
                height: 5,
            }],
        },
        AtlasDamageEpoch {
            epoch: 8,
            damage: vec![DvmGpuAtlasDamage {
                x: 30,
                y: 40,
                width: 6,
                height: 7,
            }],
        },
    ]);
    let requested = [DvmGpuAtlasDamage {
        x: 50,
        y: 60,
        width: 8,
        height: 9,
    }];
    assert_eq!(
        snapshot_damage_for_slot(6, 9, &requested, &history, 1600, 900),
        Ok(vec![
            DvmGpuAtlasDamage {
                x: 10,
                y: 20,
                width: 4,
                height: 5,
            },
            DvmGpuAtlasDamage {
                x: 30,
                y: 40,
                width: 6,
                height: 7,
            },
            DvmGpuAtlasDamage {
                x: 50,
                y: 60,
                width: 8,
                height: 9,
            },
        ])
    );
}

#[test]
fn snapshot_damage_merges_only_overlapping_history() {
    let history = VecDeque::from([AtlasDamageEpoch {
        epoch: 2,
        damage: vec![DvmGpuAtlasDamage {
            x: 10,
            y: 10,
            width: 10,
            height: 10,
        }],
    }]);
    let requested = [DvmGpuAtlasDamage {
        x: 15,
        y: 15,
        width: 10,
        height: 10,
    }];
    assert_eq!(
        snapshot_damage_for_slot(1, 3, &requested, &history, 1600, 900),
        Ok(vec![DvmGpuAtlasDamage {
            x: 10,
            y: 10,
            width: 15,
            height: 15,
        }])
    );
}

#[test]
fn slot_reconstruction_budget_rejects_atlas_amplification() {
    assert!(reconstruction_damage_within_budget(100 * 100, 1600, 900));
    assert!(reconstruction_damage_within_budget(180_000, 1600, 900));
    assert!(!reconstruction_damage_within_budget(180_001, 1600, 900));
    assert!(!reconstruction_damage_within_budget(1600 * 900, 1600, 900));
}

#[test]
fn difference_bounds_matches_a_per_pixel_scan() {
    fn per_pixel(
        previous: &[u32],
        next: &[u32],
        width: usize,
        height: usize,
        stride: usize,
    ) -> Option<Rect> {
        let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
        for y in 0..height {
            for x in 0..width {
                let index = y * stride + x;
                if previous.get(index) != next.get(index) {
                    left = left.min(x);
                    top = top.min(y);
                    right = right.max(x + 1);
                    bottom = bottom.max(y + 1);
                }
            }
        }
        (left < right && top < bottom).then(|| Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }
    let (width, height, stride) = (37_usize, 23_usize, 41_usize);
    let base: Vec<u32> = (0..stride * height).map(|i| i as u32 * 7).collect();
    for (x, y) in [(0, 0), (36, 22), (5, 9), (36, 0), (0, 22)] {
        let mut next = base.clone();
        next[y * stride + x] ^= 1;
        assert_eq!(
            difference_bounds(&base, &next, width, height, stride),
            per_pixel(&base, &next, width, height, stride),
            "pixel ({x}, {y})"
        );
    }
    let mut next = base.clone();
    next[3 * stride + 30] ^= 1;
    next[17 * stride + 2] ^= 1;
    assert_eq!(
        difference_bounds(&base, &next, width, height, stride),
        per_pixel(&base, &next, width, height, stride)
    );
    // Padding between rows is not part of the image.
    let mut padded = base.clone();
    padded[4 * stride + width + 1] ^= 1;
    assert_eq!(
        difference_bounds(&base, &padded, width, height, stride),
        None
    );
}

#[test]
fn queued_frame_deadline_starts_at_predecessor_completion() {
    let submitted_at = Instant::now();
    let completed_at = submitted_at + Duration::from_millis(40);
    assert_eq!(
        completion_wait_origin(submitted_at, completed_at),
        completed_at
    );
    assert_eq!(
        completion_wait_origin(completed_at, submitted_at),
        completed_at
    );
}

#[test]
fn repacked_fragment_damage_reconstructs_every_rotated_slot_pixel() {
    // Moving an occluder can move cropped sources in atlas space, including
    // clearing old regions. Both writes AND clears must reach an older slot.
    let width = 16;
    let height = 12;
    let mut old = vec![0; width * height];
    old[2 * width + 3..2 * width + 8].fill(0xff123456);
    let mut intermediate = old.clone();
    intermediate[2 * width + 3..2 * width + 8].fill(0);
    intermediate[5 * width + 1..5 * width + 6].fill(0xff123456);
    let mut desired = intermediate.clone();
    desired[10 * width + 11] = 0xff654321;
    let mut first = Vec::new();
    difference_damage_into(&old, &intermediate, width, height, width, &mut first).unwrap();
    let mut requested = Vec::new();
    difference_damage_into(&intermediate, &desired, width, height, width, &mut requested).unwrap();
    let history = VecDeque::from([AtlasDamageEpoch {
        epoch: 2,
        damage: first,
    }]);
    let damage =
        snapshot_damage_for_slot(1, 3, &requested, &history, width as u32, height as u32).unwrap();
    assert_eq!(
        damage.len(),
        3,
        "old clear, repacked content, and distant cursor stay separate"
    );
    copy_atlas_damage_to_slot(
        &mut old,
        &desired,
        width,
        &damage,
        width as u32,
        height as u32,
    )
    .unwrap();
    assert_eq!(
        old, desired,
        "cleared storage cannot reappear on slot rotation"
    );
}
