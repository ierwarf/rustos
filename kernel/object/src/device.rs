#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceId {
    Console,
    Display,
    Input,
}

impl DeviceId {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Console => "/dev/console0",
            Self::Display => "/dev/display0",
            Self::Input => "/dev/input0",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceAccessKind {
    Native,
    Evdev,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceHandle {
    device_id: DeviceId,
    access_kind: DeviceAccessKind,
    token: u64,
}

impl DeviceHandle {
    pub fn with_access(device_id: DeviceId, access_kind: DeviceAccessKind) -> Self {
        static NEXT_TOKEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        let token = NEXT_TOKEN
            .fetch_update(
                core::sync::atomic::Ordering::Relaxed,
                core::sync::atomic::Ordering::Relaxed,
                |next| next.checked_add(1),
            )
            .expect("device open-description token exhausted");
        Self {
            device_id,
            access_kind,
            token,
        }
    }

    pub fn from_parts_with_token(
        device_id: DeviceId,
        access_kind: DeviceAccessKind,
        token: u64,
    ) -> Self {
        assert!(token != 0, "device open-description token must be nonzero");
        Self {
            device_id,
            access_kind,
            token,
        }
    }

    pub const fn device_id(self) -> DeviceId {
        self.device_id
    }

    pub const fn access_kind(self) -> DeviceAccessKind {
        self.access_kind
    }

    pub const fn token_id(self) -> u64 {
        self.token
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceAccessKind, DeviceHandle, DeviceId};

    #[test]
    fn every_device_open_description_has_a_distinct_nonzero_token() {
        let first = DeviceHandle::with_access(DeviceId::Input, DeviceAccessKind::Evdev);
        let second = DeviceHandle::with_access(DeviceId::Input, DeviceAccessKind::Evdev);
        assert_ne!(first.token_id(), 0);
        assert_ne!(second.token_id(), 0);
        assert_ne!(first.token_id(), second.token_id());
    }
}
