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
