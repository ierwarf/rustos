use nucleus_core::util::ring::RingBuffer;

use crate::sync::KernelWaitLock;

const OUTPUT_BUFFER_CAPACITY: usize = 4096;

static CONSOLE: KernelWaitLock<ConsoleState> = KernelWaitLock::new(ConsoleState::new());

#[allow(dead_code)]
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
