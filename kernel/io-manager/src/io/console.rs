// RING3-MIGRATION-REFERENCE START: bootstrap exception: sessiond/runtimed own
// console routing and presentation policy. Ring0 keeps bootstrap/system
// console buffer substrate only.
use nucleus_core::util::ring::RingBuffer;

use crate::sync::KernelWaitLock;

const OUTPUT_BUFFER_CAPACITY: usize = 4096;

static CONSOLE: KernelWaitLock<
    ConsoleState,
    { nucleus_core::util::lockdep::LockClass::ConsoleWait as u8 },
> = KernelWaitLock::new(ConsoleState::new());

pub fn write(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    CONSOLE.lock().system.extend_overwrite(bytes)
}

struct ConsoleState {
    system: RingBuffer<u8, OUTPUT_BUFFER_CAPACITY>,
}

impl ConsoleState {
    const fn new() -> Self {
        Self {
            system: RingBuffer::new(),
        }
    }
}
// RING3-MIGRATION-REFERENCE END: sessiond/runtimed-owned console bootstrap substrate.
