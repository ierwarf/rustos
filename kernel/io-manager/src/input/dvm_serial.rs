// RING3-MIGRATION-REFERENCE START: DVM input relay transport exception.
// L0 owns DVM admission and event validation; inputd owns Linux-keymap,
// modifier, text, and session policy. Ring0 only polls the dedicated virtual
// UART and carries bounded, sequenced frames into the existing ingress queue.
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::KernelSpinLock as Mutex;
use driver_abi::PointerPacket;
use rustos_user_abi::ui::{INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED};
use x86_64::instructions::port::Port;

const DATA_PORT: u16 = 0x02f8;
const INTERRUPT_ENABLE_PORT: u16 = DATA_PORT + 1;
const FIFO_CONTROL_PORT: u16 = DATA_PORT + 2;
const LINE_CONTROL_PORT: u16 = DATA_PORT + 3;
const MODEM_CONTROL_PORT: u16 = DATA_PORT + 4;
const LINE_STATUS_PORT: u16 = DATA_PORT + 5;
const LINE_STATUS_DATA_READY: u8 = 1 << 0;
const LINE_STATUS_ABSENT: u8 = u8::MAX;
const FRAME_BYTES: usize = 32;
// `inputd` polls the authenticated DVM ingress every 4ms.  Drain up to sixteen
// complete RDI2 frames per request so a high-rate pointer never waits for the
// housekeeping task's unrelated wakeups, while keeping this ring0 transport
// operation bounded.
const MAX_BYTES_PER_TURN: usize = FRAME_BYTES * 16;
const MAGIC: [u8; 4] = *b"RDI1";
const VERSION: u8 = 2;
const KIND_SESSION_START: u8 = 0;
const KIND_KEY: u8 = 1;
const KIND_POINTER: u8 = 2;
const KIND_SESSION_END: u8 = 3;
const LINUX_EVDEV_KEY_MAX: u16 = 0x02ff;
const POINTER_BUTTON_MASK: u8 = 0x1f;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static DECODER: Mutex<Decoder> = Mutex::new(Decoder::new());

struct Decoder {
    bytes: [u8; FRAME_BYTES],
    len: usize,
    epoch: u32,
    sequence: u32,
}

impl Decoder {
    const fn new() -> Self {
        Self {
            bytes: [0; FRAME_BYTES],
            len: 0,
            epoch: 0,
            sequence: 0,
        }
    }

    fn feed(&mut self, byte: u8) -> usize {
        if self.len < FRAME_BYTES {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
        if self.len != FRAME_BYTES {
            return 0;
        }
        if !self.frame_is_well_formed() {
            self.bytes.copy_within(1..FRAME_BYTES, 0);
            self.len = FRAME_BYTES - 1;
            return 0;
        }
        let accepted = self.consume_frame();
        self.len = 0;
        accepted as usize
    }

    fn frame_is_well_formed(&self) -> bool {
        self.bytes[..4] == MAGIC
            && self.bytes[4] == VERSION
            && self.bytes[6] == 0
            && self.bytes[7] == 0
            && u32::from_be_bytes(self.bytes[28..32].try_into().expect("frame checksum"))
                == crc32(&self.bytes[..28])
    }

    fn consume_frame(&mut self) -> bool {
        let kind = self.bytes[5];
        let epoch = u32::from_be_bytes(self.bytes[8..12].try_into().expect("frame epoch"));
        let sequence = u32::from_be_bytes(self.bytes[12..16].try_into().expect("frame sequence"));
        match kind {
            KIND_SESSION_START
                if epoch != 0
                    && sequence == 0
                    && self.bytes[16..28].iter().all(|&byte| byte == 0) =>
            {
                self.epoch = epoch;
                self.sequence = 0;
                false
            }
            KIND_KEY
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && self.bytes[16..18] != [0, 0]
                    && self.bytes[19..28].iter().all(|&byte| byte == 0) =>
            {
                let code =
                    u16::from_be_bytes(self.bytes[16..18].try_into().expect("frame key code"));
                let value = self.bytes[18];
                if code > LINUX_EVDEV_KEY_MAX {
                    return false;
                }
                let action = match value {
                    0 => INPUT_ACTION_RELEASED,
                    1 => INPUT_ACTION_PRESSED,
                    2 => INPUT_ACTION_REPEATED,
                    _ => return false,
                };
                self.sequence = sequence;
                crate::input::event_queue::submit_dvm_linux_key(action, code)
            }
            KIND_POINTER
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && self.bytes[25..28].iter().all(|&byte| byte == 0)
                    && self.bytes[24] & !POINTER_BUTTON_MASK == 0 =>
            {
                self.sequence = sequence;
                crate::input::event_queue::submit_dvm_pointer_packet(PointerPacket {
                    buttons: self.bytes[24],
                    reserved0: 0,
                    reserved1: 0,
                    reserved2: 0,
                    dx: i16::from_be_bytes(self.bytes[16..18].try_into().expect("frame dx")),
                    dy: i16::from_be_bytes(self.bytes[18..20].try_into().expect("frame dy")),
                    wheel_vertical: i16::from_be_bytes(
                        self.bytes[20..22].try_into().expect("frame vertical wheel"),
                    ),
                    wheel_horizontal: i16::from_be_bytes(
                        self.bytes[22..24]
                            .try_into()
                            .expect("frame horizontal wheel"),
                    ),
                })
            }
            KIND_SESSION_END
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && self.bytes[16..28].iter().all(|&byte| byte == 0) =>
            {
                self.epoch = 0;
                self.sequence = 0;
                crate::input::event_queue::submit_dvm_input_reset();
                false
            }
            _ => false,
        }
    }
}

pub(crate) fn init() {
    let status = unsafe {
        let mut port = Port::<u8>::new(LINE_STATUS_PORT);
        port.read()
    };
    if status == LINE_STATUS_ABSENT {
        return;
    }
    unsafe {
        let mut interrupt_enable = Port::<u8>::new(INTERRUPT_ENABLE_PORT);
        let mut line_control = Port::<u8>::new(LINE_CONTROL_PORT);
        let mut data = Port::<u8>::new(DATA_PORT);
        let mut fifo_control = Port::<u8>::new(FIFO_CONTROL_PORT);
        let mut modem_control = Port::<u8>::new(MODEM_CONTROL_PORT);
        // Dedicated COM2 transport, 115200 8N1, IRQs intentionally disabled.
        // The authenticated inputd ingest broker is the sole runtime drain
        // owner and polls this bounded transport on its regular 4ms cadence.
        interrupt_enable.write(0);
        line_control.write(0x80);
        data.write(1);
        interrupt_enable.write(0);
        line_control.write(0x03);
        fifo_control.write(0xc7);
        modem_control.write(0x0b);
    }
    ACTIVE.store(true, Ordering::Release);
    crate::debug::println!("DVM input relay transport ready: COM2 framed ingress");
}

pub(crate) fn service_pending() -> usize {
    if !ACTIVE.load(Ordering::Acquire) {
        return 0;
    }
    let mut accepted = 0;
    let mut decoder = DECODER.lock();
    for _ in 0..MAX_BYTES_PER_TURN {
        let status = unsafe {
            let mut port = Port::<u8>::new(LINE_STATUS_PORT);
            port.read()
        };
        if status & LINE_STATUS_DATA_READY == 0 {
            break;
        }
        let byte = unsafe {
            let mut port = Port::<u8>::new(DATA_PORT);
            port.read()
        };
        accepted += decoder.feed(byte);
    }
    accepted
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg() & 0xedb8_8320;
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{
        Decoder, FRAME_BYTES, KIND_KEY, KIND_POINTER, KIND_SESSION_END, KIND_SESSION_START, MAGIC,
        VERSION, crc32,
    };

    fn frame(kind: u8, epoch: u32, sequence: u32, code: u16, value: u8) -> [u8; FRAME_BYTES] {
        let mut bytes = [0_u8; FRAME_BYTES];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4] = VERSION;
        bytes[5] = kind;
        bytes[8..12].copy_from_slice(&epoch.to_be_bytes());
        bytes[12..16].copy_from_slice(&sequence.to_be_bytes());
        bytes[16..18].copy_from_slice(&code.to_be_bytes());
        bytes[18] = value;
        let checksum = crc32(&bytes[..28]).to_be_bytes();
        bytes[28..32].copy_from_slice(&checksum);
        bytes
    }

    #[test]
    fn decoder_requires_session_and_monotonic_sequence() {
        let mut decoder = Decoder::new();
        let session = frame(KIND_SESSION_START, 7, 0, 0, 0);
        let first_key = frame(KIND_KEY, 7, 1, 30, 1);
        for byte in session {
            assert_eq!(decoder.feed(byte), 0);
        }
        assert_eq!(decoder.epoch, 7);
        assert_eq!(decoder.sequence, 0);
        decoder.bytes = first_key;
        decoder.len = FRAME_BYTES;
        assert!(decoder.frame_is_well_formed());
        assert_eq!(
            u32::from_be_bytes(decoder.bytes[12..16].try_into().unwrap()),
            decoder.sequence + 1
        );
    }

    #[test]
    fn decoder_resynchronizes_after_bad_checksum() {
        let mut decoder = Decoder::new();
        let mut malformed = frame(KIND_SESSION_START, 11, 0, 0, 0);
        malformed[28] ^= 1;
        let session = frame(KIND_SESSION_START, 11, 0, 0, 0);
        for byte in malformed.into_iter().chain(session) {
            let _ = decoder.feed(byte);
        }
        assert_eq!(decoder.epoch, 11);
        assert_eq!(decoder.sequence, 0);
    }

    #[test]
    fn decoder_accepts_pointer_packet_and_resets_on_session_end() {
        let mut decoder = Decoder::new();
        let session = frame(KIND_SESSION_START, 19, 0, 0, 0);
        let mut pointer = frame(KIND_POINTER, 19, 1, 5, 0);
        pointer[18..20].copy_from_slice(&(-3_i16).to_be_bytes());
        pointer[20..22].copy_from_slice(&(1_i16).to_be_bytes());
        pointer[24] = 1;
        let checksum = crc32(&pointer[..28]).to_be_bytes();
        pointer[28..32].copy_from_slice(&checksum);
        let end = frame(KIND_SESSION_END, 19, 2, 0, 0);
        for byte in session.into_iter().chain(pointer).chain(end) {
            let _ = decoder.feed(byte);
        }
        assert_eq!(decoder.epoch, 0);
        assert_eq!(decoder.sequence, 0);
    }
}
// RING3-MIGRATION-REFERENCE END: DVM input relay transport exception.
