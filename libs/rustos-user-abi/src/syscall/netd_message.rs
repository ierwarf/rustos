//! Netd v8 message envelope. Addresses are copied bytes, never user pointers.
//! The four native-endian u32 fields are data length, control length, message
//! flags and address length, followed by data, control and a sockaddr_in.

pub const HEADER_SIZE: usize = 16;
pub const IPV4_ADDRESS_SIZE: usize = 16;

#[derive(Debug, Eq, PartialEq)]
pub struct Message<'a> {
    pub data: &'a [u8],
    pub control: &'a [u8],
    pub flags: u32,
    pub address: &'a [u8],
}

pub fn decode(bytes: &[u8]) -> Option<Message<'_>> {
    let header = bytes.get(..HEADER_SIZE)?;
    let field = |offset| {
        u32::from_ne_bytes(header[offset..offset + 4].try_into().ok()?)
            .try_into()
            .ok()
    };
    let data_len: usize = field(0)?;
    let control_len: usize = field(4)?;
    let address_len: usize = field(12)?;
    if address_len != 0 && address_len != IPV4_ADDRESS_SIZE {
        return None;
    }
    let control_start = HEADER_SIZE.checked_add(data_len)?;
    let address_start = control_start.checked_add(control_len)?;
    let end = address_start.checked_add(address_len)?;
    if end != bytes.len() {
        return None;
    }
    Some(Message {
        data: bytes.get(HEADER_SIZE..control_start)?,
        control: bytes.get(control_start..address_start)?,
        flags: u32::from_ne_bytes(header[8..12].try_into().ok()?),
        address: bytes.get(address_start..end)?,
    })
}

pub fn encode(
    output: &mut [u8],
    data: &[u8],
    control: &[u8],
    flags: u32,
    address: &[u8],
) -> Option<usize> {
    if !address.is_empty() && address.len() != IPV4_ADDRESS_SIZE {
        return None;
    }
    let control_start = HEADER_SIZE.checked_add(data.len())?;
    let address_start = control_start.checked_add(control.len())?;
    let end = address_start.checked_add(address.len())?;
    let output = output.get_mut(..end)?;
    for (index, value) in [data.len(), control.len(), flags as usize, address.len()]
        .into_iter()
        .enumerate()
    {
        let value = u32::try_from(value).ok()?;
        output[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
    }
    output[HEADER_SIZE..control_start].copy_from_slice(data);
    output[control_start..address_start].copy_from_slice(control);
    output[address_start..end].copy_from_slice(address);
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressed_message_round_trip_and_empty_datagram() {
        let mut output = [0; 64];
        let address = [7; IPV4_ADDRESS_SIZE];
        for data in [&b"hello"[..], &b""[..]] {
            let len = encode(&mut output, data, &[], 0x20, &address).unwrap();
            let decoded = decode(&output[..len]).unwrap();
            assert_eq!(decoded.data, data);
            assert!(decoded.control.is_empty());
            assert_eq!(decoded.address, address);
            assert_eq!(decoded.flags, 0x20);
        }
    }

    #[test]
    fn rejects_short_overflowing_trailing_and_invalid_address_envelopes() {
        assert!(decode(&[0; 15]).is_none());
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(decode(&bytes).is_none());
        bytes.fill(0);
        assert!(decode(&bytes).is_none());
        bytes[12..16].copy_from_slice(&1_u32.to_ne_bytes());
        assert!(decode(&bytes[..17]).is_none());
        assert!(encode(&mut bytes, b"x", &[], 0, &[0; 16]).is_none());
    }
}
