pub const IPC_ABI_VERSION: u32 = 1;
pub const PORT_NAME_CAPACITY: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcHeader {
    pub opcode: u32,
    pub flags: u32,
    pub txid: u64,
    pub payload_len: u32,
    pub handle_count: u16,
    pub reserved: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PortName {
    pub bytes: [u8; PORT_NAME_CAPACITY],
    pub len: u16,
    pub reserved: [u8; 6],
}

impl PortName {
    pub const fn empty() -> Self {
        Self {
            bytes: [0; PORT_NAME_CAPACITY],
            len: 0,
            reserved: [0; 6],
        }
    }

    pub fn try_from_str(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() > PORT_NAME_CAPACITY {
            return None;
        }

        let mut name = Self::empty();
        name.bytes[..bytes.len()].copy_from_slice(bytes);
        name.len = bytes.len() as u16;
        Some(name)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()).ok()
    }
}

impl Default for PortName {
    fn default() -> Self {
        Self::empty()
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PortHandle(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChannelHandle(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SharedRegionHandle(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventHandle(pub u64);
