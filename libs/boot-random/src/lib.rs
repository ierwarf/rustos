#![no_std]

use core::{
    mem::swap,
    sync::atomic::{AtomicU64, Ordering},
};

use boot_protocol::{BootInfo, rng_seed_usable};
use rand_chacha::ChaCha20Rng;
use rand_core::{Rng, SeedableRng};
use spin::Mutex;

pub struct Random {
    rng: ChaCha20Rng,
}

static RNG_SEED: Mutex<[u8; 32]> = Mutex::new([0; 32]);
static RNG_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[must_use]
pub fn init(boot_info: &BootInfo) -> bool {
    if !rng_seed_usable(boot_info.rng_seed) {
        return false;
    }
    *RNG_SEED.lock() = boot_info.rng_seed;
    true
}

impl Random {
    pub fn new() -> Self {
        let mut seed = *RNG_SEED.lock();
        let counter = RNG_INSTANCE_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .to_le_bytes();
        for (dst, src) in seed[..counter.len()].iter_mut().zip(counter) {
            *dst ^= src;
        }

        Self {
            rng: ChaCha20Rng::from_seed(seed),
        }
    }

    pub fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.rng.fill_bytes(dest);
    }

    fn uniform_below(&mut self, upper: u64) -> u64 {
        let zone = u64::MAX - (u64::MAX % upper);
        loop {
            let value = self.rng.next_u64();
            if value < zone {
                return value % upper;
            }
        }
    }

    // min <= value < max
    pub fn randint(&mut self, mut min: i64, mut max: i64) -> i64 {
        if min == max {
            return min;
        } else if min > max {
            swap(&mut min, &mut max);
        }

        let span = (max as i128 - min as i128) as u64;
        let offset = self.uniform_below(span) as i128;

        (min as i128 + offset) as i64
    }
}

impl Default for Random {
    fn default() -> Self {
        Self::new()
    }
}
