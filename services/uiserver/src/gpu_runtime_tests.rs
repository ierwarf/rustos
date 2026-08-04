// SPDX-License-Identifier: MIT

use super::{
    copy_atlas_damage_to_slot, difference_bounds, gpu_completion_timed_out, gpu_provider_admission,
    gpu_ready_retry_backoff, next_frame_deadline, reconstruction_damage_within_budget,
    snapshot_damage_for_slot, AtlasDamageEpoch, DvmGpuAtlasDamage, GpuCompositorRuntime,
    GpuProviderAdmission, Rect, GPU_COMPLETION_TIMEOUT, GPU_FIRST_FRAME_TIMEOUT,
    GPU_FRAME_INTERVAL, GPU_INITIALIZATION_RETAINS_BOOT_CLASS, GPU_PROVIDER_HEALTH_INTERVAL,
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
fn frame_deadline_skips_missed_slots_without_drift_or_burst() {
    let origin = Instant::now();
    assert_eq!(
        next_frame_deadline(origin, origin),
        origin + GPU_FRAME_INTERVAL
    );
    assert_eq!(
        next_frame_deadline(origin, origin + Duration::from_millis(49)),
        origin + Duration::from_millis(60)
    );
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
