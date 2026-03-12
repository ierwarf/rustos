use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::ring::RingBuffer;

const OUTPUT_BUFFER_CAPACITY: usize = 4096;

static CONSOLE: Mutex<ConsoleState> = Mutex::new(ConsoleState::new());

pub fn init() {
    crate::gui::init_console();
}

#[allow(dead_code)]
pub fn write(bytes: &[u8]) -> usize {
    interrupts::without_interrupts(|| CONSOLE.lock().write(bytes))
}

pub(crate) fn write_from_tty(bytes: &[u8]) -> usize {
    CONSOLE.lock().write(bytes)
}

#[allow(dead_code)]
pub fn copy_recent_output(dest: &mut [u8]) -> usize {
    interrupts::without_interrupts(|| CONSOLE.lock().copy_recent_output(dest))
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *CONSOLE.lock() = ConsoleState::new();
}

#[cfg(test)]
pub(crate) fn copy_recent_output_for_tests(dest: &mut [u8]) -> usize {
    CONSOLE.lock().copy_recent_output(dest)
}

struct ConsoleState {
    output: RingBuffer<u8, OUTPUT_BUFFER_CAPACITY>,
    backend: ConsoleOutput,
}

impl ConsoleState {
    const fn new() -> Self {
        Self {
            output: RingBuffer::new(),
            backend: ConsoleOutput::new(),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> usize {
        self.write_output(bytes)
    }

    fn copy_recent_output(&self, dest: &mut [u8]) -> usize {
        self.output.copy_into(dest)
    }

    fn write_output(&mut self, bytes: &[u8]) -> usize {
        let written = self.output.extend_overwrite(bytes);
        self.backend.write(bytes);
        written
    }
}

enum ConsoleOutput {
    Gui(GuiConsoleOutput),
}

impl ConsoleOutput {
    const fn new() -> Self {
        Self::Gui(GuiConsoleOutput)
    }

    fn write(&mut self, bytes: &[u8]) {
        match self {
            Self::Gui(output) => output.write(bytes),
        }
    }
}

struct GuiConsoleOutput;

impl GuiConsoleOutput {
    fn write(&mut self, bytes: &[u8]) {
        crate::gui::write_console(bytes);
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
