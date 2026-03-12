use core::arch::x86_64::{_rdrand64_step, _rdseed64_step, _rdtsc};
use core::hint::spin_loop;

use raw_cpuid::CpuId;
use uefi::boot;
use uefi::proto::rng::Rng;

use crate::debug;

pub fn generate_seed(
    front_addr: u64,
    front_size: u64,
    back_addr: u64,
    back_size: u64,
) -> [u8; 32] {
    debug::println!("bootloader: random: trying UEFI RNG");
    if let Some(seed) = try_seed_from_uefi_rng() {
        debug::println!("bootloader: random: using UEFI RNG");
        return seed;
    }

    debug::println!("bootloader: random: trying CPU RNG");
    if let Some(seed) = try_seed_from_cpu_rng() {
        debug::println!("bootloader: random: using CPU RNG");
        return seed;
    }

    debug::println!("bootloader: random: falling back to boot-time entropy mix");
    fallback_seed(front_addr, front_size, back_addr, back_size)
}

fn try_seed_from_uefi_rng() -> Option<[u8; 32]> {
    let handle = boot::get_handle_for_protocol::<Rng>().ok()?;
    let mut rng = boot::open_protocol_exclusive::<Rng>(handle).ok()?;
    let mut seed = [0u8; 32];
    rng.get_rng(None, &mut seed).ok()?;
    Some(seed)
}

fn try_seed_from_cpu_rng() -> Option<[u8; 32]> {
    let cpuid = CpuId::new();

    if cpuid
        .get_extended_feature_info()
        .is_some_and(|features| features.has_rdseed())
    {
        if let Some(seed) = unsafe { seed_from_rdseed() } {
            return Some(seed);
        }
    }

    if cpuid
        .get_feature_info()
        .is_some_and(|features| features.has_rdrand())
    {
        if let Some(seed) = unsafe { seed_from_rdrand() } {
            return Some(seed);
        }
    }

    None
}

#[target_feature(enable = "rdseed")]
unsafe fn seed_from_rdseed() -> Option<[u8; 32]> {
    seed_from_cpu_step(_rdseed64_step)
}

#[target_feature(enable = "rdrand")]
unsafe fn seed_from_rdrand() -> Option<[u8; 32]> {
    seed_from_cpu_step(_rdrand64_step)
}

unsafe fn seed_from_cpu_step(step: unsafe fn(&mut u64) -> i32) -> Option<[u8; 32]> {
    let mut seed = [0u8; 32];
    for chunk in seed.chunks_exact_mut(core::mem::size_of::<u64>()) {
        let mut value = 0u64;
        let mut success = false;
        for _ in 0..32 {
            if unsafe { step(&mut value) } == 1 {
                success = true;
                break;
            }
            spin_loop();
        }
        if !success {
            return None;
        }
        chunk.copy_from_slice(&value.to_le_bytes());
    }

    Some(seed)
}

fn fallback_seed(front_addr: u64, front_size: u64, back_addr: u64, back_size: u64) -> [u8; 32] {
    let mut seed = [0u8; 32];
    let mut state = front_addr
        ^ front_size.rotate_left(7)
        ^ back_addr.rotate_left(13)
        ^ back_size.rotate_left(29)
        ^ unsafe { _rdtsc() };

    for chunk in seed.chunks_exact_mut(core::mem::size_of::<u64>()) {
        state = splitmix64(state);
        chunk.copy_from_slice(&state.to_le_bytes());
    }

    seed
}

fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
