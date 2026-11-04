use super::{
    LINUX_EVDEV_KEY_MAX, RUSTOS_INPUT_FRAME_BYTES, RUSTOS_INPUT_KIND_KEY,
    RUSTOS_INPUT_KIND_POINTER, RUSTOS_INPUT_MAGIC, RUSTOS_INPUT_VERSION,
    RUSTOS_POINTER_BUTTON_MASK, RustosInputFrame,
};

fn assert_header(bytes: &[u8; RUSTOS_INPUT_FRAME_BYTES], kind: u8, epoch: u32, sequence: u32) {
    assert!(bytes[0] == RUSTOS_INPUT_MAGIC[0]);
    assert!(bytes[1] == RUSTOS_INPUT_MAGIC[1]);
    assert!(bytes[2] == RUSTOS_INPUT_MAGIC[2]);
    assert!(bytes[3] == RUSTOS_INPUT_MAGIC[3]);
    assert!(bytes[4] == RUSTOS_INPUT_VERSION);
    assert!(bytes[5] == kind);
    let epoch_bytes = epoch.to_be_bytes();
    assert!(bytes[8] == epoch_bytes[0]);
    assert!(bytes[9] == epoch_bytes[1]);
    assert!(bytes[10] == epoch_bytes[2]);
    assert!(bytes[11] == epoch_bytes[3]);
    let sequence_bytes = sequence.to_be_bytes();
    assert!(bytes[12] == sequence_bytes[0]);
    assert!(bytes[13] == sequence_bytes[1]);
    assert!(bytes[14] == sequence_bytes[2]);
    assert!(bytes[15] == sequence_bytes[3]);
}

#[kani::proof]
fn key_frame_has_exact_provenance_bounds_and_checksum() {
    let epoch: u32 = kani::any();
    let sequence: u32 = kani::any();
    let code: u16 = kani::any();
    let value: u8 = kani::any();
    let result = RustosInputFrame::linux_evdev_key(epoch, sequence, code, value);
    kani::cover!(result.is_ok());
    kani::cover!(result.is_err());

    if epoch == 0 || sequence == 0 || code == 0 || code > LINUX_EVDEV_KEY_MAX || value > 2 {
        assert!(result.is_err());
    } else if let Ok(frame) = result {
        let bytes = frame.as_bytes();
        assert_header(bytes, RUSTOS_INPUT_KIND_KEY, epoch, sequence);
        let code_bytes = code.to_be_bytes();
        assert!(bytes[16] == code_bytes[0]);
        assert!(bytes[17] == code_bytes[1]);
        assert!(bytes[18] == value);
        if epoch == 7 && sequence == 3 && code == 30 && value == 1 {
            assert!(bytes[28] == 0x40);
            assert!(bytes[29] == 0xf2);
            assert!(bytes[30] == 0x19);
            assert!(bytes[31] == 0x2d);
        }
    } else {
        assert!(false);
    }
}

#[kani::proof]
fn pointer_frame_rejects_unknown_button_bits_and_preserves_wire_fields() {
    let epoch: u32 = kani::any();
    let sequence: u32 = kani::any();
    let dx: i16 = kani::any();
    let dy: i16 = kani::any();
    let wheel_vertical: i16 = kani::any();
    let wheel_horizontal: i16 = kani::any();
    let buttons: u8 = kani::any();
    let result = RustosInputFrame::linux_evdev_pointer(
        epoch,
        sequence,
        dx,
        dy,
        wheel_vertical,
        wheel_horizontal,
        buttons,
    );
    kani::cover!(result.is_ok());
    kani::cover!(result.is_err());

    if epoch == 0 || sequence == 0 || buttons & !RUSTOS_POINTER_BUTTON_MASK != 0 {
        assert!(result.is_err());
    } else if let Ok(frame) = result {
        let bytes = frame.as_bytes();
        assert_header(bytes, RUSTOS_INPUT_KIND_POINTER, epoch, sequence);
        let dx_bytes = dx.to_be_bytes();
        assert!(bytes[16] == dx_bytes[0]);
        assert!(bytes[17] == dx_bytes[1]);
        let dy_bytes = dy.to_be_bytes();
        assert!(bytes[18] == dy_bytes[0]);
        assert!(bytes[19] == dy_bytes[1]);
        let wheel_vertical_bytes = wheel_vertical.to_be_bytes();
        assert!(bytes[20] == wheel_vertical_bytes[0]);
        assert!(bytes[21] == wheel_vertical_bytes[1]);
        let wheel_horizontal_bytes = wheel_horizontal.to_be_bytes();
        assert!(bytes[22] == wheel_horizontal_bytes[0]);
        assert!(bytes[23] == wheel_horizontal_bytes[1]);
        assert!(bytes[24] == buttons);
        if epoch == 9
            && sequence == 4
            && dx == -4
            && dy == 2
            && wheel_vertical == 1
            && wheel_horizontal == 0
            && buttons == 3
        {
            assert!(bytes[28] == 0x36);
            assert!(bytes[29] == 0xf6);
            assert!(bytes[30] == 0x52);
            assert!(bytes[31] == 0x3f);
        }
    } else {
        assert!(false);
    }
}
