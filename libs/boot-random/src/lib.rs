#![no_std]

use core::mem::swap;

use boot_protocol::{BootInfo, rng_seed_usable};
use rand_chacha::ChaCha20Rng;
use rand_core::{Rng, SeedableRng};
use spin::Mutex;

pub struct Random {
    rng: ChaCha20Rng,
}

/// Boot-seeded master stream. It is used only to derive independent child
/// seeds; consumers never receive or clone the master state.
static RNG_MASTER: Mutex<Option<ChaCha20Rng>> = Mutex::new(None);

#[must_use]
pub fn init(boot_info: &BootInfo) -> bool {
    if !rng_seed_usable(boot_info.rng_seed) {
        return false;
    }
    let mut master = RNG_MASTER.lock();
    if master.is_some() {
        return false;
    }
    *master = Some(ChaCha20Rng::from_seed(boot_info.rng_seed));
    true
}

impl Random {
    pub fn new() -> Self {
        let mut master = RNG_MASTER.lock();
        #[cfg(feature = "deterministic-test-seed")]
        if master.is_none() {
            *master = Some(ChaCha20Rng::from_seed([
                0x52, 0x75, 0x73, 0x74, 0x4f, 0x53, 0x2d, 0x74, 0x65, 0x73, 0x74, 0x2d, 0x6f, 0x6e,
                0x6c, 0x79, 0x2d, 0x43, 0x53, 0x50, 0x52, 0x4e, 0x47, 0x2d, 0x73, 0x65, 0x65, 0x64,
                0x2d, 0x76, 0x31, 0x21,
            ]));
        }
        let seed = derive_child_seed(
            master
                .as_mut()
                .expect("CSPRNG used before one-time boot entropy initialization"),
        );
        drop(master);
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

fn derive_child_seed(master: &mut ChaCha20Rng) -> [u8; 32] {
    let mut seed = [0_u8; 32];
    master.fill_bytes(&mut seed);
    seed
}

impl Default for Random {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_streams_are_derived_from_private_master_output() {
        let mut master = ChaCha20Rng::from_seed([0x5a; 32]);
        let first = derive_child_seed(&mut master);
        let second = derive_child_seed(&mut master);
        assert_ne!(first, [0; 32]);
        assert_ne!(second, [0; 32]);
        assert_ne!(first, second);
    }
}
