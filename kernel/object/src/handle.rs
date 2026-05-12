#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleOwner {
    Ipc,
    Io,
    Compat,
    Ps,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandleToken {
    owner: HandleOwner,
    object_id: u64,
}

impl HandleToken {
    pub const fn new(owner: HandleOwner, object_id: u64) -> Self {
        Self { owner, object_id }
    }

    pub const fn owner(self) -> HandleOwner {
        self.owner
    }

    pub const fn object_id(self) -> u64 {
        self.object_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileHandleRights(u32);

impl FileHandleRights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const APPEND: Self = Self(1 << 2);
    pub const NONBLOCK: Self = Self(1 << 3);
    pub const TRANSFER: Self = Self(1 << 4);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceHandleRights(u32);

impl DeviceHandleRights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const IOCTL: Self = Self(1 << 2);
    pub const ADMIN: Self = Self(1 << 3);
    pub const MAP: Self = Self(1 << 4);
    pub const TRANSFER: Self = Self(1 << 5);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedRegionRights(u32);

impl SharedRegionRights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const MAP: Self = Self(1 << 2);
    pub const TRANSFER: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketHandleRights(u32);

impl SocketHandleRights {
    pub const SEND: Self = Self(1 << 0);
    pub const RECV: Self = Self(1 << 1);
    pub const PASS_FD: Self = Self(1 << 2);
    pub const TRANSFER: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, right: Self) -> bool {
        self.0 & right.0 == right.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleRights {
    Console,
    File(FileHandleRights),
    Device(DeviceHandleRights),
    SharedRegion(SharedRegionRights),
    Socket(SocketHandleRights),
    Epoll,
    Memfd(FileHandleRights),
    DisplaySurface(SharedRegionRights),
}

impl HandleRights {
    pub const fn allows_transfer(self) -> bool {
        match self {
            Self::File(rights) | Self::Memfd(rights) => rights.contains(FileHandleRights::TRANSFER),
            Self::Socket(rights) => rights.contains(SocketHandleRights::TRANSFER),
            Self::Device(rights) => rights.contains(DeviceHandleRights::TRANSFER),
            Self::SharedRegion(rights) | Self::DisplaySurface(rights) => {
                rights.contains(SharedRegionRights::TRANSFER)
            }
            Self::Console | Self::Epoll => false,
        }
    }

    pub const fn allows_shared_map(self) -> bool {
        match self {
            Self::SharedRegion(rights) | Self::DisplaySurface(rights) => {
                rights.contains(SharedRegionRights::MAP)
            }
            Self::Device(rights) => rights.contains(DeviceHandleRights::MAP),
            _ => false,
        }
    }

    pub const fn allows_read(self) -> bool {
        match self {
            Self::File(rights) | Self::Memfd(rights) => rights.contains(FileHandleRights::READ),
            Self::Device(rights) => rights.contains(DeviceHandleRights::READ),
            Self::SharedRegion(rights) | Self::DisplaySurface(rights) => {
                rights.contains(SharedRegionRights::READ)
            }
            _ => false,
        }
    }

    pub const fn allows_write(self) -> bool {
        match self {
            Self::File(rights) | Self::Memfd(rights) => rights.contains(FileHandleRights::WRITE),
            Self::Device(rights) => rights.contains(DeviceHandleRights::WRITE),
            Self::SharedRegion(rights) | Self::DisplaySurface(rights) => {
                rights.contains(SharedRegionRights::WRITE)
            }
            _ => false,
        }
    }

    pub const fn allows_device_ioctl(self) -> bool {
        match self {
            Self::Device(rights) => rights.contains(DeviceHandleRights::IOCTL),
            _ => false,
        }
    }

    pub const fn allows_device_admin(self) -> bool {
        match self {
            Self::Device(rights) => rights.contains(DeviceHandleRights::ADMIN),
            _ => false,
        }
    }
}
