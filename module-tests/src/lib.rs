#![allow(dead_code)]

extern crate alloc;

mod pic {
    pub fn enable_irq(_irq: u8) {}
}

mod debug {
    pub fn write_bytes(_bytes: &[u8]) {}
}

#[path = "../../kernel/src/io/session.rs"]
mod session;

mod gui {
    use core::sync::atomic::{AtomicBool, AtomicI16, AtomicUsize, Ordering};

    use crate::session::ConsoleSessionId;

    static MOUSE_VISIBLE: AtomicBool = AtomicBool::new(false);
    static MOUSE_LEFT_BUTTON: AtomicBool = AtomicBool::new(false);
    static MOUSE_MOVE_X: AtomicI16 = AtomicI16::new(0);
    static MOUSE_MOVE_Y: AtomicI16 = AtomicI16::new(0);
    static MOUSE_SHOW_COUNT: AtomicUsize = AtomicUsize::new(0);

    pub fn init_console() {}

    pub fn write_console(_bytes: &[u8]) {}

    pub fn write_console_session(_session: ConsoleSessionId, _bytes: &[u8]) {}

    pub fn try_write_console(_bytes: &[u8]) -> bool {
        true
    }

    pub fn tick_console_cursor() {}

    pub fn sync_desktop_windows() {}

    pub fn focused_console_session() -> ConsoleSessionId {
        ConsoleSessionId::PRIMARY
    }

    pub fn show_mouse_cursor() -> bool {
        MOUSE_VISIBLE.store(true, Ordering::Release);
        MOUSE_SHOW_COUNT.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn move_mouse_cursor_relative(dx: i16, dy: i16) -> bool {
        MOUSE_MOVE_X.store(dx, Ordering::Release);
        MOUSE_MOVE_Y.store(dy, Ordering::Release);
        true
    }

    pub fn set_mouse_left_button(pressed: bool) -> bool {
        MOUSE_LEFT_BUTTON.store(pressed, Ordering::Release);
        true
    }

    #[allow(dead_code)]
    pub fn reset_mouse_state() {
        MOUSE_VISIBLE.store(false, Ordering::Release);
        MOUSE_LEFT_BUTTON.store(false, Ordering::Release);
        MOUSE_MOVE_X.store(0, Ordering::Release);
        MOUSE_MOVE_Y.store(0, Ordering::Release);
        MOUSE_SHOW_COUNT.store(0, Ordering::Release);
    }

    #[allow(dead_code)]
    pub fn mouse_visible() -> bool {
        MOUSE_VISIBLE.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn last_mouse_move() -> (i16, i16) {
        (
            MOUSE_MOVE_X.load(Ordering::Acquire),
            MOUSE_MOVE_Y.load(Ordering::Acquire),
        )
    }

    #[allow(dead_code)]
    pub fn mouse_show_count() -> usize {
        MOUSE_SHOW_COUNT.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn last_mouse_left_button() -> bool {
        MOUSE_LEFT_BUTTON.load(Ordering::Acquire)
    }
}

mod desktop {
    pub fn sync_all_console_windows() {}
}

mod ui_service {
    use crate::keyboard::KeyboardEvent;

    pub fn push_keyboard_event(_event: KeyboardEvent) {}
    pub fn push_pointer_motion(_dx: i16, _dy: i16) {}
    pub fn push_pointer_button_left(_pressed: bool) {}
}

mod multitask {
    pub struct Thread;

    impl Thread {
        pub fn new(_entry: fn(u64), _weight_micros: u64) -> Self {
            Self
        }

        pub fn start(&self) {}
    }

    pub fn service_deferred_work() -> usize {
        0
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

#[path = "../../kernel/src/input/mouse.rs"]
mod mouse;

#[path = "../../kernel/src/storage/fat.rs"]
mod fat;

#[path = "../../kernel/src/arch/pit.rs"]
mod pit;

#[path = "../../kernel/src/arch/rtc.rs"]
mod rtc;

#[path = "../../prekernel/src/load/elf_loader.rs"]
mod prekernel_elf_loader;
