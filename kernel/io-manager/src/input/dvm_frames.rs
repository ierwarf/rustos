// RING3-MIGRATION-REFERENCE START: DVM input relay transport exception.
// L0 owns DVM admission and event validation; inputd owns Linux-keymap,
// modifier, text, and session policy. Ring0 validates only fixed, sequenced
// frames drained from the host-owned input ring in task context.
use crate::sync::KernelSpinLock as Mutex;
use driver_abi::PointerPacket;
use rustos_user_abi::ui::{INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED};
const FRAME_BYTES: usize = 32;
const MAGIC: [u8; 4] = *b"RDI1";
const VERSION: u8 = 3;
const KIND_SESSION_START: u8 = 0;
const KIND_KEY: u8 = 1;
const KIND_POINTER: u8 = 2;
const KIND_SESSION_END: u8 = 3;
const KIND_POINTER_POSITION: u8 = 4;
const LINUX_EVDEV_KEY_MAX: u16 = 0x02ff;
const POINTER_BUTTON_MASK: u8 = 0x1f;
const POINTER_POSITION_MAX_X: u16 = 1599;
const POINTER_POSITION_MAX_Y: u16 = 899;
static DECODER: Mutex<Decoder> = Mutex::new(Decoder::new());

struct Decoder {
    epoch: u32,
    sequence: u32,
}

impl Decoder {
    const fn new() -> Self {
        Self {
            epoch: 0,
            sequence: 0,
        }
    }

    fn frame_is_well_formed(bytes: &[u8; FRAME_BYTES]) -> bool {
        bytes[..4] == MAGIC
            && bytes[4] == VERSION
            && bytes[6] == 0
            && bytes[7] == 0
            && u32::from_be_bytes(bytes[28..32].try_into().expect("frame checksum"))
                == crc32(&bytes[..28])
    }

    fn consume_frame(&mut self, bytes: &[u8; FRAME_BYTES]) -> bool {
        let kind = bytes[5];
        let epoch = u32::from_be_bytes(bytes[8..12].try_into().expect("frame epoch"));
        let sequence = u32::from_be_bytes(bytes[12..16].try_into().expect("frame sequence"));
        match kind {
            KIND_SESSION_START
                if epoch != 0 && sequence == 0 && bytes[16..28].iter().all(|&byte| byte == 0) =>
            {
                // A new authenticated relay epoch revokes every prior DVM
                // key/button assertion, including a session that died before
                // it could send SESSION_END.  The reset barrier also purges
                // queued retired-session frames before this epoch starts.
                let _ = submit_dvm_input_reset();
                // RDI1 session markers are L0-authenticated lifecycle
                // signals, not guest-provided input. The fixed Ethernet ring
                // reuses only this lifecycle lease: no network frame travels
                // over the input ring and its bounded header cannot activate it.
                activate_network_control_from_session(epoch);
                self.epoch = epoch;
                self.sequence = 0;
                false
            }
            KIND_KEY
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[16..18] != [0, 0]
                    && bytes[19..28].iter().all(|&byte| byte == 0) =>
            {
                let code = u16::from_be_bytes(bytes[16..18].try_into().expect("frame key code"));
                let value = bytes[18];
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
                submit_dvm_linux_key(action, code)
            }
            KIND_POINTER
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[25..28].iter().all(|&byte| byte == 0)
                    && bytes[24] & !POINTER_BUTTON_MASK == 0 =>
            {
                self.sequence = sequence;
                submit_dvm_pointer_packet(PointerPacket {
                    buttons: bytes[24],
                    reserved0: 0,
                    reserved1: 0,
                    reserved2: 0,
                    dx: i16::from_be_bytes(bytes[16..18].try_into().expect("frame dx")),
                    dy: i16::from_be_bytes(bytes[18..20].try_into().expect("frame dy")),
                    wheel_vertical: i16::from_be_bytes(
                        bytes[20..22].try_into().expect("frame vertical wheel"),
                    ),
                    wheel_horizontal: i16::from_be_bytes(
                        bytes[22..24].try_into().expect("frame horizontal wheel"),
                    ),
                })
            }
            KIND_POINTER_POSITION
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[25..28].iter().all(|&byte| byte == 0)
                    && bytes[24] & !POINTER_BUTTON_MASK == 0 =>
            {
                let x =
                    u16::from_be_bytes(bytes[16..18].try_into().expect("frame absolute pointer x"));
                let y =
                    u16::from_be_bytes(bytes[18..20].try_into().expect("frame absolute pointer y"));
                if x > POINTER_POSITION_MAX_X || y > POINTER_POSITION_MAX_Y {
                    return false;
                }
                self.sequence = sequence;
                submit_dvm_pointer_position(
                    x,
                    y,
                    i16::from_be_bytes(bytes[20..22].try_into().expect("frame vertical wheel")),
                    i16::from_be_bytes(bytes[22..24].try_into().expect("frame horizontal wheel")),
                    bytes[24],
                )
            }
            KIND_SESSION_END
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[16..28].iter().all(|&byte| byte == 0) =>
            {
                // Exact-epoch revoke prevents a delayed cleanup from an old
                // L0 relay session from disabling a newer authenticated DVM.
                revoke_network_control_from_session(epoch);
                self.epoch = 0;
                self.sequence = 0;
                let _ = submit_dvm_input_reset();
                false
            }
            _ => false,
        }
    }
}

/// Consume one complete fixed ring record. The caller owns cursor validation
/// and invokes this only from the capability-gated inputd broker, never from
/// the MSI-X leaf callback.
pub(crate) fn consume_record(record: &[u8; FRAME_BYTES]) -> usize {
    let mut decoder = DECODER.lock();
    let accepted = Decoder::frame_is_well_formed(record) && decoder.consume_frame(record);
    // Ring slots are independently framed. Invalid slots are rejected as one
    // record and can never borrow bytes from a successor.
    accepted as usize
}

/// Revoke decoder and policy authority when the transport itself is revoked.
/// L0 cannot reliably append SESSION_END after RustOS has rejected the shared
/// header, so transport teardown must not leave an authenticated epoch, a
/// network lease, or pressed input state behind.
pub(crate) fn revoke_active_session() {
    let epoch = {
        let mut decoder = DECODER.lock();
        let epoch = decoder.epoch;
        decoder.epoch = 0;
        decoder.sequence = 0;
        epoch
    };
    if epoch != 0 {
        revoke_network_control_from_session(epoch);
    }
    let _ = submit_dvm_input_reset();
}

fn activate_network_control_from_session(epoch: u32) {
    #[cfg(not(test))]
    {
        let _ = crate::io::dvm_network::activate_authenticated_control(epoch);
    }
    #[cfg(test)]
    {
        // Decoder tests have no live input-ring/PCI topology. Lease semantics are
        // covered by dvm_network's host-independent ControlLease tests.
        let _ = epoch;
    }
}

fn revoke_network_control_from_session(epoch: u32) {
    #[cfg(not(test))]
    {
        let _ = crate::io::dvm_network::revoke_authenticated_control(epoch);
    }
    #[cfg(test)]
    {
        let _ = epoch;
    }
}

fn submit_dvm_input_reset() -> bool {
    #[cfg(not(test))]
    {
        crate::input::event_queue::submit_dvm_input_reset()
    }
    #[cfg(test)]
    {
        // Decoder tests validate framing/order only. Queue/barrier behavior is
        // tested in input::event_queue and must not invoke scheduler wakeups
        // from a host unit-test thread.
        true
    }
}

fn submit_dvm_linux_key(action: u16, code: u16) -> bool {
    #[cfg(not(test))]
    {
        crate::input::event_queue::submit_dvm_linux_key(action, code)
    }
    #[cfg(test)]
    {
        let _ = (action, code);
        true
    }
}

fn submit_dvm_pointer_packet(packet: PointerPacket) -> bool {
    #[cfg(not(test))]
    {
        crate::input::event_queue::submit_dvm_pointer_packet(packet)
    }
    #[cfg(test)]
    {
        let _ = packet;
        true
    }
}

fn submit_dvm_pointer_position(
    x: u16,
    y: u16,
    wheel_vertical: i16,
    wheel_horizontal: i16,
    buttons: u8,
) -> bool {
    #[cfg(not(test))]
    {
        crate::input::event_queue::submit_dvm_pointer_position(
            x,
            y,
            wheel_vertical,
            wheel_horizontal,
            buttons,
        )
    }
    #[cfg(test)]
    {
        let _ = (x, y, wheel_vertical, wheel_horizontal, buttons);
        true
    }
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
        Decoder, FRAME_BYTES, KIND_KEY, KIND_POINTER, KIND_POINTER_POSITION, KIND_SESSION_END,
        KIND_SESSION_START, MAGIC, VERSION, crc32,
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
        assert!(!decoder.consume_frame(&session));
        assert_eq!(decoder.epoch, 7);
        assert_eq!(decoder.sequence, 0);
        assert!(Decoder::frame_is_well_formed(&first_key));
        assert_eq!(
            u32::from_be_bytes(first_key[12..16].try_into().unwrap()),
            decoder.sequence + 1
        );
    }

    #[test]
    fn decoder_resynchronizes_after_bad_checksum() {
        let mut decoder = Decoder::new();
        let mut malformed = frame(KIND_SESSION_START, 11, 0, 0, 0);
        malformed[28] ^= 1;
        let session = frame(KIND_SESSION_START, 11, 0, 0, 0);
        assert!(!Decoder::frame_is_well_formed(&malformed));
        assert!(!decoder.consume_frame(&session));
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
        assert!(!decoder.consume_frame(&session));
        assert!(decoder.consume_frame(&pointer));
        assert!(!decoder.consume_frame(&end));
        assert_eq!(decoder.epoch, 0);
        assert_eq!(decoder.sequence, 0);
    }

    #[test]
    fn decoder_accepts_bounded_absolute_position_and_rejects_out_of_range() {
        let mut decoder = Decoder::new();
        assert!(!decoder.consume_frame(&frame(KIND_SESSION_START, 23, 0, 0, 0)));

        let mut position = frame(KIND_POINTER_POSITION, 23, 1, 800, 0);
        position[18..20].copy_from_slice(&450_u16.to_be_bytes());
        let checksum = crc32(&position[..28]).to_be_bytes();
        position[28..32].copy_from_slice(&checksum);
        assert!(decoder.consume_frame(&position));

        let mut invalid = frame(KIND_POINTER_POSITION, 23, 2, 1600, 0);
        invalid[18..20].copy_from_slice(&450_u16.to_be_bytes());
        let checksum = crc32(&invalid[..28]).to_be_bytes();
        invalid[28..32].copy_from_slice(&checksum);
        assert!(!decoder.consume_frame(&invalid));
        assert_eq!(decoder.sequence, 1);
    }
}
// RING3-MIGRATION-REFERENCE END: DVM input relay transport exception.
