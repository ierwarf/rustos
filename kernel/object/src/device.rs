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
}

impl DeviceHandle {
    pub const fn with_access(device_id: DeviceId, access_kind: DeviceAccessKind) -> Self {
        Self {
            device_id,
            access_kind,
        }
    }

    pub const fn device_id(self) -> DeviceId {
        self.device_id
    }

    pub const fn access_kind(self) -> DeviceAccessKind {
        self.access_kind
    }
}
