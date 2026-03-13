use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::ring::RingBuffer;

const OUTPUT_BUFFER_CAPACITY: usize = 4096;
const PENDING_BUFFER_CAPACITY: usize = 4096;
const FLUSH_CHUNK_CAPACITY: usize = 256;
const CONSOLE_THREAD_WEIGHT_MICROS: u64 = 10;

static CONSOLE: Mutex<ConsoleState> = Mutex::new(ConsoleState::new());
static CONSOLE_THREAD: Mutex<Option<crate::multitask::Thread>> = Mutex::new(None);
static CONSOLE_THREAD_STARTED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    crate::gui::init_console();
}

pub fn start_worker() {
    interrupts::without_interrupts(|| {
        if CONSOLE_THREAD_STARTED.load(Ordering::Acquire) {
            return;
        }

        let thread =
            crate::multitask::Thread::new(console_thread_main, CONSOLE_THREAD_WEIGHT_MICROS);
        thread.start();
        *CONSOLE_THREAD.lock() = Some(thread);
        CONSOLE_THREAD_STARTED.store(true, Ordering::Release);
    });
}

#[allow(dead_code)]
pub fn write(bytes: &[u8]) -> usize {
    let written = interrupts::without_interrupts(|| CONSOLE.lock().write(bytes));
    if !CONSOLE_THREAD_STARTED.load(Ordering::Acquire) {
        while flush_pending_once() {}
    }
    written
}

pub(crate) fn write_from_tty(bytes: &[u8]) -> usize {
    let written = CONSOLE.lock().write(bytes);
    if !CONSOLE_THREAD_STARTED.load(Ordering::Acquire) {
        while flush_pending_once() {}
    }
    written
}

#[allow(dead_code)]
pub fn copy_recent_output(dest: &mut [u8]) -> usize {
    interrupts::without_interrupts(|| CONSOLE.lock().copy_recent_output(dest))
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *CONSOLE.lock() = ConsoleState::new();
    *CONSOLE_THREAD.lock() = None;
    CONSOLE_THREAD_STARTED.store(false, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn copy_recent_output_for_tests(dest: &mut [u8]) -> usize {
    CONSOLE.lock().copy_recent_output(dest)
}

fn flush_pending_once() -> bool {
    let mut chunk = [0_u8; FLUSH_CHUNK_CAPACITY];
    let len = CONSOLE.lock().drain_pending(&mut chunk);
    if len == 0 {
        return false;
    }

    crate::gui::write_console(&chunk[..len]);
    true
}

fn console_thread_main(_id: u64) {
    loop {
        let mut drained = false;
        while flush_pending_once() {
            drained = true;
        }

        if !drained {
            crate::gui::tick_console_cursor();
            interrupts::enable_and_hlt();
        }
    }
}

struct ConsoleState {
    output: RingBuffer<u8, OUTPUT_BUFFER_CAPACITY>,
    pending: RingBuffer<u8, PENDING_BUFFER_CAPACITY>,
}

impl ConsoleState {
    const fn new() -> Self {
        Self {
            output: RingBuffer::new(),
            pending: RingBuffer::new(),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }

        let written = self.output.extend_overwrite(bytes);
        self.pending.extend_overwrite(bytes);
        written
    }

    fn drain_pending(&mut self, dest: &mut [u8]) -> usize {
        self.pending.pop_into(dest)
    }

    fn copy_recent_output(&self, dest: &mut [u8]) -> usize {
        self.output.copy_into(dest)
    }
}

#[cfg(test)]
mod tests {
    use super::ConsoleState;

    #[test]
    fn stores_recent_output() {
        let mut console = ConsoleState::new();
        assert_eq!(console.write(b"asdf\r\n"), 6);

        let mut output = [0_u8; 8];
        assert_eq!(console.copy_recent_output(&mut output), 6);
        assert_eq!(&output[..6], b"asdf\r\n");
    }
}
