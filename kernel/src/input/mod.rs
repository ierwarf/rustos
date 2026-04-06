use crate::driver;
use driver_abi::{DriverBus, DriverClass};
use spin::Mutex;

pub(crate) mod dispatcher;
pub(crate) mod event_queue;
pub(crate) mod i8042;
pub(crate) mod keyboard;

const KEYBOARD_DRIVER_NAME: &str = "rustos-keyboard";
// QEMU TCG is sensitive to long deferred waits during early userspace/input
// bring-up. Zero-delay deadlines still defer work to the next service pass
// without depending on RTC progress.
const AUX_TRANSPORT_START_DELAY_MS: u64 = 0;
const LOADABLE_MOUSE_DRIVER_START_DELAY_MS: u64 = 0;

static DEFERRED_INPUT_STATE: Mutex<DeferredInputState> = Mutex::new(DeferredInputState::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeferredInputStage {
    WaitingForUserspaceDisplay,
    WaitingForAuxBringUp,
    WaitingForLoadableMouseDriver,
    Completed,
}

struct DeferredInputState {
    stage: DeferredInputStage,
    deadline_tick: u64,
}

impl DeferredInputState {
    const fn new() -> Self {
        Self {
            stage: DeferredInputStage::WaitingForUserspaceDisplay,
            deadline_tick: 0,
        }
    }
}

pub fn init() {
    driver::register_kernel_builtin(KEYBOARD_DRIVER_NAME, DriverClass::Input, DriverBus::Serio);
    report_keyboard_transport(i8042::init_keyboard_port());
}

pub fn on_keyboard_interrupt() {
    i8042::on_keyboard_interrupt();
}

pub fn on_mouse_interrupt() {
    i8042::on_aux_interrupt();
}

pub fn service_pending() -> usize {
    enum Action {
        None,
        ScheduleAuxBringUp,
        SkipAuxBringUp,
        InitializeAuxTransport,
        InitializeLoadableModules,
    }

    let mut work = i8042::service_pending();
    work += dispatcher::service_pending();

    let action = {
        let mut state = DEFERRED_INPUT_STATE.lock();
        match state.stage {
            DeferredInputStage::WaitingForUserspaceDisplay => {
                if !crate::io::gui::is_userspace_display_active() {
                    Action::None
                } else if crate::usb::has_runtime_pointer_device() {
                    state.stage = DeferredInputStage::Completed;
                    Action::SkipAuxBringUp
                } else {
                    state.stage = DeferredInputStage::WaitingForAuxBringUp;
                    state.deadline_tick = deadline_after_ms(AUX_TRANSPORT_START_DELAY_MS);
                    Action::ScheduleAuxBringUp
                }
            }
            DeferredInputStage::WaitingForAuxBringUp => {
                if crate::arch::rtc::ticks() < state.deadline_tick {
                    Action::None
                } else {
                    Action::InitializeAuxTransport
                }
            }
            DeferredInputStage::WaitingForLoadableMouseDriver => {
                if crate::arch::rtc::ticks() < state.deadline_tick {
                    Action::None
                } else {
                    state.stage = DeferredInputStage::Completed;
                    Action::InitializeLoadableModules
                }
            }
            DeferredInputStage::Completed => Action::None,
        }
    };

    match action {
        Action::None => work,
        Action::ScheduleAuxBringUp => {
            crate::debug::println!(
                "Deferred input service: userspace display active, aux bring-up scheduled"
            );
            work += 1;
            work
        }
        Action::SkipAuxBringUp => {
            crate::debug::println!(
                "Deferred input service: skipping PS/2 aux bring-up because a USB pointer is already present"
            );
            work += 1;
            work
        }
        Action::InitializeAuxTransport => {
            let aux_ready = initialize_deferred_aux_transport();
            let mut state = DEFERRED_INPUT_STATE.lock();
            state.stage = if aux_ready {
                state.deadline_tick = deadline_after_ms(LOADABLE_MOUSE_DRIVER_START_DELAY_MS);
                DeferredInputStage::WaitingForLoadableMouseDriver
            } else {
                DeferredInputStage::Completed
            };
            work += 1;
            work
        }
        Action::InitializeLoadableModules => {
            crate::debug::println!("Deferred input service: loading serio mouse modules");
            driver::initialize_loadable_modules_for_bus(DriverBus::Serio);
            work += 1;
            work
        }
    }
}

fn deadline_after_ms(milliseconds: u64) -> u64 {
    if milliseconds == 0 {
        return crate::arch::rtc::ticks();
    }

    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let ticks_needed = (milliseconds.saturating_mul(ticks_per_second) + 999) / 1000;
    let ticks_needed = core::cmp::max(1, ticks_needed);
    crate::arch::rtc::ticks().saturating_add(ticks_needed)
}

fn report_keyboard_transport(result: i8042::KeyboardTransportInitResult) {
    match result {
        i8042::KeyboardTransportInitResult::Ready(info) => {
            keyboard::configure_scancode_transport(info.translated);
            crate::debug::println!(
                "PS/2 keyboard transport ready: translated={}, self_test={}, port_test={}",
                info.translated,
                info.controller_self_test_passed,
                info.first_port_test_passed,
            );
            crate::io::console::write(b"PS/2 keyboard transport ready.\r\n");
        }
        i8042::KeyboardTransportInitResult::Unavailable(_reason) => {
            crate::debug::println!("PS/2 keyboard transport unavailable: {}", _reason);
            crate::io::console::write(b"PS/2 keyboard transport unavailable.\r\n");
        }
    }
}

fn report_aux_transport(result: i8042::AuxTransportInitResult) {
    match result {
        i8042::AuxTransportInitResult::Ready(_info) => {
            crate::debug::println!(
                "PS/2 aux serio port ready: configured={}, port_test={}",
                _info.controller_configured,
                _info.second_port_test_passed,
            );
            if !crate::io::gui::is_userspace_display_active() {
                crate::io::console::write(b"PS/2 aux serio port ready.\r\n");
            }
        }
        i8042::AuxTransportInitResult::Unavailable(_reason) => {
            crate::debug::println!("PS/2 aux serio port unavailable: {}", _reason);
            if !crate::io::gui::is_userspace_display_active() {
                crate::io::console::write(b"PS/2 aux serio port unavailable.\r\n");
            }
        }
    }
}

fn initialize_deferred_aux_transport() -> bool {
    if crate::usb::has_runtime_pointer_device() {
        crate::debug::println!(
            "Deferred input service: skipping aux transport because a USB pointer is already present"
        );
        return false;
    }

    match i8042::init_aux_mouse_port() {
        i8042::AuxTransportInitResult::Ready(info) => {
            crate::debug::println!("Deferred input service: aux transport ready");
            report_aux_transport(i8042::AuxTransportInitResult::Ready(info));
            true
        }
        i8042::AuxTransportInitResult::Unavailable(reason) => {
            crate::debug::println!(
                "Deferred input service: aux transport unavailable: {}",
                reason
            );
            report_aux_transport(i8042::AuxTransportInitResult::Unavailable(reason));
            false
        }
    }
}
