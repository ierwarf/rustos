pub mod device {
    pub use crate::device::{DeviceAccessKind, DeviceHandle, DeviceId};
}

pub mod handle {
    pub use crate::handle::{
        DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken,
        SharedRegionRights, SocketHandleRights,
    };
}

pub mod session {
    pub use crate::session::ConsoleSessionHandle;
}

pub use device::{DeviceAccessKind, DeviceHandle, DeviceId};
pub use handle::{
    DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken,
    SharedRegionRights, SocketHandleRights,
};
pub use session::ConsoleSessionHandle;
