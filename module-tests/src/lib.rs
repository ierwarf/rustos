#![allow(dead_code)]

extern crate alloc;

mod pic {
    pub fn enable_irq(_irq: u8) {}
}

mod debug {
    pub fn write_bytes(_bytes: &[u8]) {}
}

mod gui {
    pub fn init_console() {}

    pub fn write_console(_bytes: &[u8]) {}

    pub fn try_write_console(_bytes: &[u8]) -> bool {
        true
    }

    pub fn tick_console_cursor() {}
}

mod multitask {
    pub struct Thread;

    impl Thread {
        pub fn new(_entry: fn(u64), _weight_micros: u64) -> Self {
            Self
        }

        pub fn start(&self) {}
    }
}

#[path = "../../kernel/src/util/ring.rs"]
mod ring;

#[path = "../../kernel/src/io/console.rs"]
mod console;

#[path = "../../kernel/src/io/tty.rs"]
mod tty;

#[path = "../../kernel/src/input/keyboard.rs"]
mod keyboard;

#[path = "../../kernel/src/storage/fat.rs"]
mod fat;

#[path = "../../kernel/src/arch/pit.rs"]
mod pit;

#[path = "../../kernel/src/arch/rtc.rs"]
mod rtc;

#[path = "../../prekernel/src/load/elf_loader.rs"]
mod prekernel_elf_loader;
