// RING3-MIGRATION-REFERENCE START: inputd/driverd should own the non-.ko i8042
// service-driver once a ring3 service-driver host can drive PS/2 command bytes
// and IRQ delivery through narrow leases. Ring0 keeps this controller path as
// privileged legacy input substrate until that host exists.
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::KernelSpinLock as Mutex;
use driver_abi::{
    POINTER_BUTTON_LEFT, POINTER_BUTTON_MIDDLE, POINTER_BUTTON_RIGHT, PointerPacket, SerioDeviceId,
    SerioDriverRegistration, SerioPortInfo,
};
use nucleus_core::util::ring::RingBuffer;
use x86_64::instructions::{interrupts, port::Port};

use crate::driver::serio::SerioPortOps;

const KEYBOARD_IRQ: u8 = 1;
const MOUSE_IRQ: u8 = 12;
#[allow(dead_code)]
const RTC_TIMER_IRQ: u8 = 8;
const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const STATUS_OUTPUT_READY: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
const STATUS_AUX_OUTPUT: u8 = 1 << 5;
// QEMU/KVM-backed PS/2 devices either respond quickly or not at all. Long
// busy-wait budgets here stall the whole machine during failed psmouse probe
// sequences because controller access is serialized and temporarily
// non-preemptible.
const I8042_IO_TIMEOUT_SPINS: usize = 256;
const I8042_PORT_TEST_TIMEOUT_SPINS: usize = 256;
const I8042_DRAIN_TIMEOUT_SPINS_PER_MS: usize = 10_000;
const I8042_DRAIN_LIMIT: usize = 256;
const I8042_READ_CONFIG: u8 = 0x20;
const I8042_WRITE_CONFIG: u8 = 0x60;
const I8042_SELF_TEST: u8 = 0xAA;
const I8042_ENABLE_SECOND_PORT: u8 = 0xA8;
const I8042_TEST_SECOND_PORT: u8 = 0xA9;
const I8042_TEST_FIRST_PORT: u8 = 0xAB;
const I8042_DISABLE_FIRST_PORT: u8 = 0xAD;
const I8042_ENABLE_FIRST_PORT: u8 = 0xAE;
const I8042_DISABLE_SECOND_PORT: u8 = 0xA7;
const I8042_WRITE_SECOND_PORT_DATA: u8 = 0xD4;
const I8042_CONFIG_IRQ1_ENABLE: u8 = 1 << 0;
const I8042_CONFIG_IRQ12_ENABLE: u8 = 1 << 1;
const I8042_CONFIG_FIRST_PORT_CLOCK_DISABLE: u8 = 1 << 4;
const I8042_CONFIG_SECOND_PORT_CLOCK_DISABLE: u8 = 1 << 5;
const I8042_CONFIG_TRANSLATION: u8 = 1 << 6;
const I8042_SELF_TEST_PASSED: u8 = 0x55;
const I8042_FIRST_PORT_TEST_PASSED: u8 = 0x00;
const I8042_SECOND_PORT_TEST_PASSED: u8 = 0x00;
const DEVICE_RESPONSE_ACK: u8 = 0xFA;
const DEVICE_RESPONSE_RESEND: u8 = 0xFE;
const DEVICE_RESPONSE_SELF_TEST_PASSED: u8 = 0xAA;
const DEVICE_SEND_RETRIES: usize = 1;
const DEVICE_RESPONSE_READ_RETRIES: usize = 1;
const AUX_COMMAND_NOISE_BUDGET: usize = 32;
const MAX_BYTES_PER_INTERRUPT: usize = 32;
const DEFERRED_CONTROLLER_BYTES_CAPACITY: usize = 256;
const DEFERRED_CONTROLLER_DROP_LOG_INTERVAL: u64 = 64;
const PS2_CMD_GETID: u8 = 0xF2;
const PS2_CMD_DISABLE_SCANNING: u8 = 0xF5;

#[allow(dead_code)]
pub(crate) const I8042_KEYBOARD_PORT_ID: u32 = 0;
pub(crate) const I8042_AUX_MOUSE_PORT_ID: u32 = 1;

static KEYBOARD_TRANSPORT_ACTIVE: AtomicBool = AtomicBool::new(false);
static AUX_TRANSPORT_ACTIVE: AtomicBool = AtomicBool::new(false);
static CONTROLLER_ACCESS_ACTIVE: AtomicBool = AtomicBool::new(false);
static AUX_INTERRUPT_SUPPRESSED: AtomicBool = AtomicBool::new(false);
static AUX_DEBUG_BYTES_REMAINING: AtomicUsize = AtomicUsize::new(0);
static CONTROLLER_ACCESS_LOCK: Mutex<()> = Mutex::new(());
static DEFERRED_CONTROLLER_BYTES: Mutex<DeferredControllerBytesState> =
    Mutex::new(DeferredControllerBytesState::new());
static PS2_MOUSE_PACKET_STATE: Mutex<Ps2MousePacketState> = Mutex::new(Ps2MousePacketState::new());

const PS2_MOUSE_STATUS_LEFT: u8 = 1 << 0;
const PS2_MOUSE_STATUS_RIGHT: u8 = 1 << 1;
const PS2_MOUSE_STATUS_MIDDLE: u8 = 1 << 2;
const PS2_MOUSE_STATUS_ALWAYS_ONE: u8 = 1 << 3;
const PS2_MOUSE_STATUS_X_OVERFLOW: u8 = 1 << 6;
const PS2_MOUSE_STATUS_Y_OVERFLOW: u8 = 1 << 7;

struct DeferredControllerBytesState {
    queued: RingBuffer<ControllerByte, DEFERRED_CONTROLLER_BYTES_CAPACITY>,
    dropped_bytes: u64,
}

struct Ps2MousePacketState {
    bytes: [u8; 3],
    len: usize,
}

impl Ps2MousePacketState {
    const fn new() -> Self {
        Self {
            bytes: [0; 3],
            len: 0,
        }
    }

    fn reset(&mut self) {
        self.bytes = [0; 3];
        self.len = 0;
    }
}

static BUILTIN_PS2_MOUSE_DRIVER: SerioDriverRegistration = SerioDriverRegistration::new(
    "rustos-ps2-mouse",
    SerioDeviceId::i8042_mouse(),
    Some(builtin_ps2_mouse_connect),
    Some(builtin_ps2_mouse_disconnect),
    Some(builtin_ps2_mouse_interrupt),
);

impl DeferredControllerBytesState {
    const fn new() -> Self {
        Self {
            queued: RingBuffer::new(),
            dropped_bytes: 0,
        }
    }

    fn push_deferred_byte(&mut self, byte: ControllerByte) {
        if self.queued.push(byte) {
            return;
        }

        self.dropped_bytes = self.dropped_bytes.saturating_add(1);
        if self.dropped_bytes % DEFERRED_CONTROLLER_DROP_LOG_INTERVAL == 0 {
            crate::debug::println!(
                "i8042 deferred byte overflow: dropped={} queued={}",
                self.dropped_bytes,
                self.queued.len()
            );
        }
    }

    fn pop_into(&mut self, dest: &mut [ControllerByte]) -> usize {
        self.queued.pop_into(dest)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyboardTransportInfo {
    pub translated: bool,
    pub controller_configured: bool,
    pub controller_self_test_passed: bool,
    pub first_port_test_passed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyboardTransportInitResult {
    Ready(KeyboardTransportInfo),
    Unavailable(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuxTransportInfo {
    pub controller_configured: bool,
    pub second_port_test_passed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxTransportInitResult {
    Ready(AuxTransportInfo),
    Unavailable(&'static str),
}

pub(crate) fn init_keyboard_port() -> KeyboardTransportInitResult {
    let result = with_controller_access(init_keyboard_port_inner);
    match result {
        Ok(info) => {
            KEYBOARD_TRANSPORT_ACTIVE.store(true, Ordering::Release);
            crate::arch::pic::enable_irq(KEYBOARD_IRQ);
            KeyboardTransportInitResult::Ready(info)
        }
        Err(reason) => {
            KEYBOARD_TRANSPORT_ACTIVE.store(false, Ordering::Release);
            KeyboardTransportInitResult::Unavailable(reason)
        }
    }
}

pub(crate) fn init_aux_mouse_port() -> AuxTransportInitResult {
    let result = with_controller_access(init_aux_mouse_port_inner);
    match result {
        Ok(info) => {
            AUX_TRANSPORT_ACTIVE.store(false, Ordering::Release);
            crate::driver::serio::register_port_with_ops(
                SerioPortInfo::i8042_mouse(I8042_AUX_MOUSE_PORT_ID),
                SerioPortOps {
                    open: Some(serio_open_aux_port),
                    close: Some(serio_close_aux_port),
                    write_byte: Some(serio_write_aux_byte),
                    ps2_command: Some(serio_aux_ps2_command),
                    drain: Some(serio_drain_aux),
                },
            );
            AuxTransportInitResult::Ready(info)
        }
        Err(reason) => {
            AUX_TRANSPORT_ACTIVE.store(false, Ordering::Release);
            AuxTransportInitResult::Unavailable(reason)
        }
    }
}

pub(crate) fn register_builtin_ps2_mouse_driver() {
    unsafe {
        let _ = crate::driver::serio::register_driver(&BUILTIN_PS2_MOUSE_DRIVER as *const _);
    }
}

pub(crate) fn on_keyboard_interrupt() {
    if !KEYBOARD_TRANSPORT_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    interrupts::without_interrupts(capture_controller_outputs);
}

pub(crate) fn on_aux_interrupt() {
    if !AUX_TRANSPORT_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    interrupts::without_interrupts(capture_controller_outputs);
}

fn init_keyboard_port_inner() -> Result<KeyboardTransportInfo, &'static str> {
    let _ = write_command(I8042_DISABLE_SECOND_PORT);
    let _ = write_command(I8042_DISABLE_FIRST_PORT);
    drain_output_buffer();

    let controller_self_test_passed = controller_self_test();
    let config = read_controller_config().ok_or("i8042 config read timed out")?;
    let translated = config & I8042_CONFIG_TRANSLATION != 0;
    let controller_configured = update_controller_config(true, false, true, false).is_some();
    let first_port_test_passed = first_port_test();
    if !write_command(I8042_ENABLE_FIRST_PORT) {
        return Err("i8042 enable-first-port command timed out");
    }
    drain_output_buffer();

    Ok(KeyboardTransportInfo {
        translated,
        controller_configured,
        controller_self_test_passed,
        first_port_test_passed,
    })
}

fn init_aux_mouse_port_inner() -> Result<AuxTransportInfo, &'static str> {
    if !write_command(I8042_ENABLE_SECOND_PORT) {
        return Err("i8042 enable-second-port command timed out");
    }

    let keyboard_enabled = KEYBOARD_TRANSPORT_ACTIVE.load(Ordering::Acquire);
    let controller_configured =
        update_controller_config(keyboard_enabled, false, keyboard_enabled, true).is_some();
    // Keep aux transport bring-up non-fatal and short. psmouse probing is the
    // real device detection path; spending a long time in the controller port
    // test here makes KVM bring-up look hung before userspace UI appears.
    let second_port_test_passed = second_port_test();
    if !second_port_test_passed {
        let _ = park_aux_port(keyboard_enabled);
        return Err("i8042 second-port test failed");
    }
    if !aux_device_present() {
        let _ = park_aux_port(keyboard_enabled);
        return Err("i8042 aux device not present");
    }
    drain_output_buffer();
    // Do not leave the aux device streaming while no serio driver owns it yet.
    // Early host mouse movement would otherwise queue stale bytes that can
    // poison the later psmouse probe/open command sequence.
    let parked = park_aux_port(keyboard_enabled);

    Ok(AuxTransportInfo {
        controller_configured: controller_configured && parked,
        second_port_test_passed,
    })
}

fn aux_device_present() -> bool {
    let mut id = [0_u8; 2];
    match send_aux_command_sequence(PS2_CMD_GETID, &[], &mut id) {
        Ok(()) => true,
        Err(-110) | Err(-19) => false,
        Err(_) => false,
    }
}

fn capture_controller_outputs() {
    for _ in 0..MAX_BYTES_PER_INTERRUPT {
        let Some(data) = read_controller_byte_nowait() else {
            break;
        };
        queue_deferred_controller_byte(data);
    }
}

fn drain_output_buffer() {
    for _ in 0..I8042_DRAIN_LIMIT {
        let Some(data) = read_controller_byte_nowait() else {
            break;
        };
        if !data.aux {
            dispatch_controller_byte(data);
        }
    }
}

fn read_controller_config() -> Option<u8> {
    if !write_command(I8042_READ_CONFIG) {
        return None;
    }
    read_keyboard_data_byte_blocking()
}

fn write_controller_config(value: u8) -> bool {
    write_command(I8042_WRITE_CONFIG) && write_data_byte(value)
}

fn update_controller_config(
    keyboard_irq_enabled: bool,
    aux_irq_enabled: bool,
    keyboard_clock_enabled: bool,
    aux_clock_enabled: bool,
) -> Option<u8> {
    let mut config = read_controller_config()?;
    config &= !(I8042_CONFIG_IRQ1_ENABLE
        | I8042_CONFIG_IRQ12_ENABLE
        | I8042_CONFIG_FIRST_PORT_CLOCK_DISABLE
        | I8042_CONFIG_SECOND_PORT_CLOCK_DISABLE);
    if keyboard_irq_enabled {
        config |= I8042_CONFIG_IRQ1_ENABLE;
    }
    if aux_irq_enabled {
        config |= I8042_CONFIG_IRQ12_ENABLE;
    }
    if !keyboard_clock_enabled {
        config |= I8042_CONFIG_FIRST_PORT_CLOCK_DISABLE;
    }
    if !aux_clock_enabled {
        config |= I8042_CONFIG_SECOND_PORT_CLOCK_DISABLE;
    }
    if !write_controller_config(config) {
        return None;
    }
    Some(config)
}

fn park_aux_port(keyboard_enabled: bool) -> bool {
    let _ = send_aux_command_sequence(PS2_CMD_DISABLE_SCANNING, &[], &mut []);
    let config_updated =
        update_controller_config(keyboard_enabled, false, keyboard_enabled, false).is_some();
    let port_disabled = write_command(I8042_DISABLE_SECOND_PORT);
    drain_output_buffer();
    config_updated && port_disabled
}

fn controller_self_test() -> bool {
    if !write_command(I8042_SELF_TEST) {
        return false;
    }
    matches!(
        read_keyboard_data_byte_blocking(),
        Some(I8042_SELF_TEST_PASSED)
    )
}

fn first_port_test() -> bool {
    if !write_command(I8042_TEST_FIRST_PORT) {
        return false;
    }
    matches!(
        read_keyboard_data_byte_blocking(),
        Some(I8042_FIRST_PORT_TEST_PASSED)
    )
}

fn second_port_test() -> bool {
    if !write_command(I8042_TEST_SECOND_PORT) {
        return false;
    }
    matches!(
        read_aux_data_byte_with_limit(I8042_PORT_TEST_TIMEOUT_SPINS),
        Some(I8042_SECOND_PORT_TEST_PASSED)
    )
}

#[allow(dead_code)]
fn serio_write_keyboard_byte(byte: u8) -> i32 {
    with_controller_access(|| if write_data_byte(byte) { 0 } else { -110 })
}

#[allow(dead_code)]
fn serio_keyboard_ps2_command(command: u8, data: &[u8], response: &mut [u8]) -> i32 {
    with_controller_access(
        || match send_keyboard_command_sequence(command, data, response) {
            Ok(()) => 0,
            Err(status) => status,
        },
    )
}

fn serio_write_aux_byte(byte: u8) -> i32 {
    with_controller_access(|| {
        if write_second_port_data_byte(byte) {
            0
        } else {
            -110
        }
    })
}

fn serio_aux_ps2_command(command: u8, data: &[u8], response: &mut [u8]) -> i32 {
    let aux_irq_enabled = AUX_TRANSPORT_ACTIVE.load(Ordering::Acquire);
    AUX_INTERRUPT_SUPPRESSED.store(true, Ordering::Release);
    if aux_irq_enabled {
        crate::arch::pic::disable_irq(MOUSE_IRQ);
    }
    let status =
        with_controller_access(
            || match send_aux_command_sequence(command, data, response) {
                Ok(()) => 0,
                Err(status) => status,
            },
        );
    if aux_irq_enabled && AUX_TRANSPORT_ACTIVE.load(Ordering::Acquire) {
        crate::arch::pic::enable_irq(MOUSE_IRQ);
    }
    AUX_INTERRUPT_SUPPRESSED.store(false, Ordering::Release);
    status
}

fn serio_drain_aux(max_bytes: usize, timeout_ms: u32) {
    if max_bytes == 0 {
        return;
    }

    with_controller_access(|| drain_aux_output_buffer(max_bytes, timeout_ms));
}

fn serio_open_aux_port() -> i32 {
    let status = with_controller_access(|| {
        if AUX_TRANSPORT_ACTIVE.load(Ordering::Acquire) {
            return 0;
        }
        let keyboard_enabled = KEYBOARD_TRANSPORT_ACTIVE.load(Ordering::Acquire);
        // Drop any bytes collected while the port was still parked.
        drain_output_buffer();
        if !write_command(I8042_ENABLE_SECOND_PORT) {
            return -110;
        }
        if keyboard_enabled {
            let _ = write_command(I8042_ENABLE_FIRST_PORT);
        }
        if update_controller_config(keyboard_enabled, true, keyboard_enabled, true).is_none() {
            let _ = write_command(I8042_DISABLE_SECOND_PORT);
            return -110;
        }
        // Keep unsolicited bytes away from the serio receiver until the port
        // is fully configured and drained.
        drain_output_buffer();
        crate::driver::input::reset_pointer_state();
        PS2_MOUSE_PACKET_STATE.lock().reset();
        AUX_TRANSPORT_ACTIVE.store(true, Ordering::Release);
        0
    });
    if status == 0 {
        crate::arch::pic::enable_irq(MOUSE_IRQ);
    }
    status
}

fn serio_close_aux_port() {
    crate::arch::pic::disable_irq(MOUSE_IRQ);
    with_controller_access(|| {
        if !AUX_TRANSPORT_ACTIVE.swap(false, Ordering::AcqRel) {
            return;
        }
        let keyboard_enabled = KEYBOARD_TRANSPORT_ACTIVE.load(Ordering::Acquire);
        let _ = park_aux_port(keyboard_enabled);
        crate::driver::input::reset_pointer_state();
        PS2_MOUSE_PACKET_STATE.lock().reset();
    });
}

unsafe extern "C" fn builtin_ps2_mouse_connect(port: *const SerioPortInfo) -> i32 {
    let Some(port) = (unsafe { port.as_ref() }) else {
        return -22;
    };
    PS2_MOUSE_PACKET_STATE.lock().reset();
    crate::driver::serio::open(port.port_id)
}

unsafe extern "C" fn builtin_ps2_mouse_disconnect(port: *const SerioPortInfo) {
    let Some(port) = (unsafe { port.as_ref() }) else {
        return;
    };
    crate::driver::serio::close(port.port_id);
    PS2_MOUSE_PACKET_STATE.lock().reset();
}

unsafe extern "C" fn builtin_ps2_mouse_interrupt(
    _port: *const SerioPortInfo,
    byte: u8,
    _flags: u32,
) -> i32 {
    let packet = {
        let mut state = PS2_MOUSE_PACKET_STATE.lock();
        match state.len {
            0 => {
                if byte & PS2_MOUSE_STATUS_ALWAYS_ONE == 0 {
                    return 0;
                }
                state.bytes[0] = byte;
                state.len = 1;
                return 1;
            }
            1 => {
                state.bytes[1] = byte;
                state.len = 2;
                return 1;
            }
            _ => {
                state.bytes[2] = byte;
                let packet = state.bytes;
                state.reset();
                packet
            }
        }
    };

    if packet[0] & (PS2_MOUSE_STATUS_X_OVERFLOW | PS2_MOUSE_STATUS_Y_OVERFLOW) != 0 {
        return 1;
    }

    let mut buttons = 0u8;
    if packet[0] & PS2_MOUSE_STATUS_LEFT != 0 {
        buttons |= POINTER_BUTTON_LEFT;
    }
    if packet[0] & PS2_MOUSE_STATUS_RIGHT != 0 {
        buttons |= POINTER_BUTTON_RIGHT;
    }
    if packet[0] & PS2_MOUSE_STATUS_MIDDLE != 0 {
        buttons |= POINTER_BUTTON_MIDDLE;
    }

    crate::driver::input::submit_pointer_packet(PointerPacket {
        buttons,
        dx: i16::from(packet[1] as i8),
        dy: -i16::from(packet[2] as i8),
        wheel_vertical: 0,
        wheel_horizontal: 0,
        reserved0: 0,
        reserved1: 0,
        reserved2: 0,
    });
    1
}

fn with_controller_access<T>(f: impl FnOnce() -> T) -> T {
    let _guard = CONTROLLER_ACCESS_LOCK.lock();
    interrupts::without_interrupts(|| {
        CONTROLLER_ACCESS_ACTIVE.store(true, Ordering::Release);
        let result = f();
        CONTROLLER_ACCESS_ACTIVE.store(false, Ordering::Release);
        result
    })
}

#[allow(dead_code)]
fn send_keyboard_command_sequence(
    command: u8,
    data: &[u8],
    response: &mut [u8],
) -> Result<(), i32> {
    send_keyboard_byte_and_expect_ack(command)?;
    for byte in data.iter().copied() {
        send_keyboard_byte_and_expect_ack(byte)?;
    }
    for (index, slot) in response.iter_mut().enumerate() {
        match read_keyboard_response_byte() {
            Ok(value) => *slot = value,
            Err(_status) if command == PS2_CMD_GETID && index != 0 => {
                for tail in &mut response[index..] {
                    *tail = 0;
                }
                return Ok(());
            }
            Err(status) => return Err(status),
        }
    }
    Ok(())
}

fn send_aux_command_sequence(command: u8, data: &[u8], response: &mut [u8]) -> Result<(), i32> {
    drain_aux_output_buffer(I8042_DRAIN_LIMIT, 0);
    send_aux_byte_and_expect_ack(command)?;
    for byte in data.iter().copied() {
        send_aux_byte_and_expect_ack(byte)?;
    }
    for (index, slot) in response.iter_mut().enumerate() {
        match read_aux_response_byte() {
            Ok(value) => *slot = value,
            Err(_status) if command == PS2_CMD_GETID && index != 0 => {
                for tail in &mut response[index..] {
                    *tail = 0;
                }
                return Ok(());
            }
            Err(status) => return Err(status),
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn send_keyboard_byte_and_expect_ack(byte: u8) -> Result<(), i32> {
    for _ in 0..DEVICE_SEND_RETRIES {
        if !write_data_byte(byte) {
            return Err(-110);
        }
        for _ in 0..DEVICE_RESPONSE_READ_RETRIES {
            match read_keyboard_data_byte_blocking() {
                Some(DEVICE_RESPONSE_ACK) => return Ok(()),
                Some(DEVICE_RESPONSE_RESEND) => break,
                Some(value) if is_ignorable_command_response(value) => continue,
                Some(_) => return Err(-5),
                None => return Err(-110),
            }
        }
    }
    Err(-5)
}

fn send_aux_byte_and_expect_ack(byte: u8) -> Result<(), i32> {
    for _ in 0..DEVICE_SEND_RETRIES {
        if !write_second_port_data_byte(byte) {
            return Err(-110);
        }
        let mut noise_budget = AUX_COMMAND_NOISE_BUDGET;
        for _ in 0..DEVICE_RESPONSE_READ_RETRIES {
            match read_aux_data_byte_blocking() {
                Some(DEVICE_RESPONSE_ACK) => return Ok(()),
                Some(DEVICE_RESPONSE_RESEND) => break,
                Some(value) if is_ignorable_command_response(value) => continue,
                Some(_value) if noise_budget != 0 => {
                    noise_budget -= 1;
                    continue;
                }
                Some(_value) => return Err(-5),
                None => return Err(-110),
            }
        }
    }
    Err(-5)
}

#[allow(dead_code)]
fn read_keyboard_response_byte() -> Result<u8, i32> {
    for _ in 0..DEVICE_RESPONSE_READ_RETRIES {
        match read_keyboard_data_byte_blocking() {
            Some(value) if is_ignorable_command_response(value) => continue,
            Some(value) => return Ok(value),
            None => return Err(-110),
        }
    }
    Err(-110)
}

fn read_aux_response_byte() -> Result<u8, i32> {
    for _ in 0..DEVICE_RESPONSE_READ_RETRIES {
        match read_aux_data_byte_blocking() {
            Some(value) if is_ignorable_command_response(value) => continue,
            Some(value) => return Ok(value),
            None => return Err(-110),
        }
    }
    Err(-110)
}

fn write_command(command: u8) -> bool {
    if !wait_for_input_empty() {
        return false;
    }

    unsafe {
        let mut status_port = Port::<u8>::new(STATUS_PORT);
        status_port.write(command);
    }
    true
}

fn write_data_byte(data: u8) -> bool {
    if !wait_for_input_empty() {
        return false;
    }

    unsafe {
        let mut data_port = Port::<u8>::new(DATA_PORT);
        data_port.write(data);
    }
    true
}

fn write_second_port_data_byte(data: u8) -> bool {
    write_command(I8042_WRITE_SECOND_PORT_DATA) && write_data_byte(data)
}

#[derive(Clone, Copy)]
struct ControllerByte {
    byte: u8,
    aux: bool,
}

fn read_keyboard_data_byte_blocking() -> Option<u8> {
    for _ in 0..I8042_IO_TIMEOUT_SPINS {
        let Some(data) = read_controller_byte_nowait() else {
            spin_loop();
            continue;
        };
        if !data.aux {
            return Some(data.byte);
        }
        dispatch_controller_byte(data);
    }

    None
}

fn read_aux_data_byte_blocking() -> Option<u8> {
    read_aux_data_byte_with_limit(I8042_IO_TIMEOUT_SPINS)
}

fn read_aux_data_byte_with_limit(spins: usize) -> Option<u8> {
    for _ in 0..spins {
        let Some(data) = read_controller_byte_nowait() else {
            spin_loop();
            continue;
        };
        if data.aux {
            return Some(data.byte);
        }
        dispatch_controller_byte(data);
    }

    None
}

fn drain_aux_output_buffer(max_bytes: usize, timeout_ms: u32) {
    let mut drained = 0usize;
    let mut remaining_spins =
        timeout_ms.saturating_mul(I8042_DRAIN_TIMEOUT_SPINS_PER_MS as u32) as usize;

    while drained < max_bytes {
        if let Some(data) = read_controller_byte_nowait() {
            if data.aux {
                drained += 1;
            } else {
                dispatch_controller_byte(data);
            }
            continue;
        }

        if remaining_spins == 0 {
            break;
        }
        remaining_spins -= 1;
        spin_loop();
    }
}

fn read_controller_byte_nowait() -> Option<ControllerByte> {
    let status = read_status();
    if status & STATUS_OUTPUT_READY == 0 {
        return None;
    }

    let byte = unsafe {
        let mut data_port = Port::<u8>::new(DATA_PORT);
        data_port.read()
    };
    Some(ControllerByte {
        byte,
        aux: status & STATUS_AUX_OUTPUT != 0,
    })
}

fn queue_deferred_controller_byte(data: ControllerByte) {
    DEFERRED_CONTROLLER_BYTES.lock().push_deferred_byte(data);
}

fn dispatch_controller_byte(data: ControllerByte) {
    if !interrupts::are_enabled() {
        queue_deferred_controller_byte(data);
        return;
    }
    dispatch_controller_byte_lower_half(data);
}

fn dispatch_controller_byte_lower_half(data: ControllerByte) {
    if data.aux {
        if AUX_INTERRUPT_SUPPRESSED.load(Ordering::Acquire) {
            return;
        }
        if AUX_TRANSPORT_ACTIVE.load(Ordering::Acquire) {
            let remaining = AUX_DEBUG_BYTES_REMAINING.load(Ordering::Relaxed);
            if remaining != 0
                && AUX_DEBUG_BYTES_REMAINING
                    .compare_exchange(
                        remaining,
                        remaining - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                crate::debug::println!(
                    "i8042 aux byte: data={:#x} active={} suppressed={}",
                    data.byte,
                    true,
                    false
                );
            }
            let _ = crate::driver::serio::receive_byte(I8042_AUX_MOUSE_PORT_ID, data.byte, 0);
        }
        return;
    }

    if KEYBOARD_TRANSPORT_ACTIVE.load(Ordering::Acquire) {
        crate::input::keyboard::on_scancode(data.byte);
    }
}

pub(crate) fn service_pending() -> usize {
    let mut bytes = [ControllerByte {
        byte: 0,
        aux: false,
    }; DEFERRED_CONTROLLER_BYTES_CAPACITY];
    let count =
        interrupts::without_interrupts(|| DEFERRED_CONTROLLER_BYTES.lock().pop_into(&mut bytes));
    for &byte in &bytes[..count] {
        dispatch_controller_byte_lower_half(byte);
    }
    count
}

fn wait_for_input_empty() -> bool {
    for _ in 0..I8042_IO_TIMEOUT_SPINS {
        if read_status() & STATUS_INPUT_FULL == 0 {
            return true;
        }
        spin_loop();
    }
    false
}

fn read_status() -> u8 {
    unsafe {
        let mut status_port = Port::<u8>::new(STATUS_PORT);
        status_port.read()
    }
}

const fn is_ignorable_command_response(byte: u8) -> bool {
    byte == DEVICE_RESPONSE_SELF_TEST_PASSED
}
// RING3-MIGRATION-REFERENCE END: inputd/driverd-owned non-.ko i8042 service-driver.
