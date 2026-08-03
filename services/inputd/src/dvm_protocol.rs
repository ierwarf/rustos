use rustos_user_abi::syscall::{
    INPUTD_ACCESS_NATIVE, INPUTD_DVM_RECORD_BYTES, INPUTD_DVM_RECORD_FLAG_RESET,
    INPUTD_INGRESS_FLAG_DVM_SOURCE, INPUTD_INGRESS_KIND_DVM_LINUX_KEY,
    INPUTD_INGRESS_KIND_POINTER_PACKET, INPUTD_INGRESS_KIND_POINTER_POSITION, InputDvmRecordWire,
    InputIngressWire, InputKeyboardEventWire, InputPointerPacketWire, InputPointerPositionWire,
};
use rustos_user_abi::ui::{INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED};

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DvmOutcome {
    pub(crate) event: Option<InputIngressWire>,
    pub(crate) reset_input: bool,
    pub(crate) grant_epoch: Option<u32>,
    pub(crate) revoke_epoch: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DvmDecoder {
    transport_generation: u64,
    epoch: u32,
    sequence: u32,
}

impl DvmDecoder {
    pub(crate) fn consume(&mut self, record: &InputDvmRecordWire) -> DvmOutcome {
        if record.flags == INPUTD_DVM_RECORD_FLAG_RESET
            && record.len == 0
            && record.reserved0 == 0
            && record.transport_generation != 0
        {
            return self.reset();
        }
        if record.flags != 0
            || record.len as usize != INPUTD_DVM_RECORD_BYTES
            || record.reserved0 != 0
            || record.transport_generation == 0
            || !frame_is_well_formed(&record.bytes)
        {
            return DvmOutcome::default();
        }
        if self.transport_generation != 0
            && self.transport_generation != record.transport_generation
        {
            // A generation transition is a revocation barrier. Require a new
            // session marker on a subsequent record; never combine implicit
            // revoke and new policy admission into one ambiguous operation.
            return self.reset();
        }
        self.transport_generation = record.transport_generation;
        let bytes = &record.bytes;
        let kind = bytes[5];
        let epoch = u32::from_be_bytes(bytes[8..12].try_into().expect("frame epoch"));
        let sequence = u32::from_be_bytes(bytes[12..16].try_into().expect("frame sequence"));
        match kind {
            KIND_SESSION_START
                if epoch != 0 && sequence == 0 && bytes[16..28].iter().all(|&byte| byte == 0) =>
            {
                let revoke_epoch = (self.epoch != 0).then_some(self.epoch);
                self.epoch = epoch;
                self.sequence = 0;
                DvmOutcome {
                    reset_input: true,
                    grant_epoch: Some(epoch),
                    revoke_epoch,
                    ..DvmOutcome::default()
                }
            }
            KIND_KEY
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[16..18] != [0, 0]
                    && bytes[19..28].iter().all(|&byte| byte == 0) =>
            {
                let code = u16::from_be_bytes(bytes[16..18].try_into().expect("key code"));
                if code > LINUX_EVDEV_KEY_MAX {
                    return DvmOutcome::default();
                }
                let action = match bytes[18] {
                    0 => INPUT_ACTION_RELEASED,
                    1 => INPUT_ACTION_PRESSED,
                    2 => INPUT_ACTION_REPEATED,
                    _ => return DvmOutcome::default(),
                };
                self.sequence = sequence;
                DvmOutcome {
                    event: Some(InputIngressWire {
                        kind: INPUTD_INGRESS_KIND_DVM_LINUX_KEY,
                        access: INPUTD_ACCESS_NATIVE,
                        flags: INPUTD_INGRESS_FLAG_DVM_SOURCE,
                        keyboard: InputKeyboardEventWire {
                            action,
                            reserved0: 0,
                            code: u32::from(code),
                            modifiers: 0,
                            text: 0,
                        },
                        ..InputIngressWire::default()
                    }),
                    ..DvmOutcome::default()
                }
            }
            KIND_POINTER
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[25..28].iter().all(|&byte| byte == 0)
                    && bytes[24] & !POINTER_BUTTON_MASK == 0 =>
            {
                self.sequence = sequence;
                DvmOutcome {
                    event: Some(InputIngressWire {
                        kind: INPUTD_INGRESS_KIND_POINTER_PACKET,
                        access: INPUTD_ACCESS_NATIVE,
                        flags: INPUTD_INGRESS_FLAG_DVM_SOURCE,
                        pointer_packet: InputPointerPacketWire {
                            buttons: bytes[24],
                            reserved0: [0; 3],
                            dx: i16::from_be_bytes(bytes[16..18].try_into().expect("pointer dx")),
                            dy: i16::from_be_bytes(bytes[18..20].try_into().expect("pointer dy")),
                            wheel_vertical: i16::from_be_bytes(
                                bytes[20..22].try_into().expect("pointer wheel"),
                            ),
                            wheel_horizontal: i16::from_be_bytes(
                                bytes[22..24].try_into().expect("pointer horizontal wheel"),
                            ),
                        },
                        ..InputIngressWire::default()
                    }),
                    ..DvmOutcome::default()
                }
            }
            KIND_POINTER_POSITION
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[25..28].iter().all(|&byte| byte == 0)
                    && bytes[24] & !POINTER_BUTTON_MASK == 0 =>
            {
                let x = u16::from_be_bytes(bytes[16..18].try_into().expect("pointer x"));
                let y = u16::from_be_bytes(bytes[18..20].try_into().expect("pointer y"));
                if x > POINTER_POSITION_MAX_X || y > POINTER_POSITION_MAX_Y {
                    return DvmOutcome::default();
                }
                self.sequence = sequence;
                DvmOutcome {
                    event: Some(InputIngressWire {
                        kind: INPUTD_INGRESS_KIND_POINTER_POSITION,
                        access: INPUTD_ACCESS_NATIVE,
                        flags: INPUTD_INGRESS_FLAG_DVM_SOURCE,
                        pointer_position: InputPointerPositionWire {
                            buttons: bytes[24],
                            reserved0: [0; 3],
                            x: i32::from(x),
                            y: i32::from(y),
                            wheel_vertical: i16::from_be_bytes(
                                bytes[20..22].try_into().expect("pointer wheel"),
                            ),
                            wheel_horizontal: i16::from_be_bytes(
                                bytes[22..24].try_into().expect("pointer horizontal wheel"),
                            ),
                        },
                        ..InputIngressWire::default()
                    }),
                    ..DvmOutcome::default()
                }
            }
            KIND_SESSION_END
                if epoch == self.epoch
                    && self.sequence.checked_add(1) == Some(sequence)
                    && bytes[16..28].iter().all(|&byte| byte == 0) =>
            {
                let outcome = DvmOutcome {
                    reset_input: true,
                    revoke_epoch: Some(epoch),
                    ..DvmOutcome::default()
                };
                self.epoch = 0;
                self.sequence = 0;
                outcome
            }
            _ => DvmOutcome::default(),
        }
    }

    fn reset(&mut self) -> DvmOutcome {
        let revoke_epoch = (self.epoch != 0).then_some(self.epoch);
        self.transport_generation = 0;
        self.epoch = 0;
        self.sequence = 0;
        DvmOutcome {
            reset_input: true,
            revoke_epoch,
            ..DvmOutcome::default()
        }
    }
}

fn frame_is_well_formed(bytes: &[u8; INPUTD_DVM_RECORD_BYTES]) -> bool {
    bytes[..4] == MAGIC
        && bytes[4] == VERSION
        && bytes[6] == 0
        && bytes[7] == 0
        && u32::from_be_bytes(bytes[28..32].try_into().expect("frame checksum"))
            == crc32(&bytes[..28])
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
    use super::*;

    fn record(kind: u8, epoch: u32, sequence: u32) -> InputDvmRecordWire {
        let mut wire = InputDvmRecordWire {
            transport_generation: 7,
            len: INPUTD_DVM_RECORD_BYTES as u16,
            ..InputDvmRecordWire::default()
        };
        wire.bytes[..4].copy_from_slice(&MAGIC);
        wire.bytes[4] = VERSION;
        wire.bytes[5] = kind;
        wire.bytes[8..12].copy_from_slice(&epoch.to_be_bytes());
        wire.bytes[12..16].copy_from_slice(&sequence.to_be_bytes());
        let checksum = crc32(&wire.bytes[..28]).to_be_bytes();
        wire.bytes[28..32].copy_from_slice(&checksum);
        wire
    }

    #[test]
    fn session_sequence_and_transport_reset_are_service_owned() {
        let mut decoder = DvmDecoder::default();
        let start = decoder.consume(&record(KIND_SESSION_START, 9, 0));
        assert_eq!(start.grant_epoch, Some(9));
        assert!(start.reset_input);

        let mut key = record(KIND_KEY, 9, 1);
        key.bytes[16..18].copy_from_slice(&30_u16.to_be_bytes());
        key.bytes[18] = 1;
        let checksum = crc32(&key.bytes[..28]).to_be_bytes();
        key.bytes[28..32].copy_from_slice(&checksum);
        assert!(decoder.consume(&key).event.is_some());

        let reset = decoder.consume(&InputDvmRecordWire {
            transport_generation: 7,
            flags: INPUTD_DVM_RECORD_FLAG_RESET,
            ..InputDvmRecordWire::default()
        });
        assert_eq!(reset.revoke_epoch, Some(9));
        assert!(reset.reset_input);
    }

    #[test]
    fn invalid_checksum_and_cross_generation_record_fail_closed() {
        let mut decoder = DvmDecoder::default();
        let mut start = record(KIND_SESSION_START, 11, 0);
        start.bytes[31] ^= 1;
        assert_eq!(decoder.consume(&start), DvmOutcome::default());

        assert_eq!(
            decoder
                .consume(&record(KIND_SESSION_START, 11, 0))
                .grant_epoch,
            Some(11)
        );
        let mut next = record(KIND_KEY, 11, 1);
        next.transport_generation = 8;
        let outcome = decoder.consume(&next);
        assert!(outcome.reset_input);
        assert_eq!(outcome.revoke_epoch, Some(11));
        assert!(outcome.event.is_none());
    }
}
