pub mod device {
    pub use crate::device::{DeviceAccessKind, DeviceHandle, DeviceId};
}

pub mod handle {
    pub use crate::handle::{
        DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken,
        SharedRegionRights, SocketHandleRights,
    };
}

pub mod identity {
    pub use crate::identity::{CapabilityEpochs, ObjectIdentity, ObjectKind, ObjectOwner};
}

pub mod session {
    pub use crate::session::ConsoleSessionHandle;
}

pub use device::{DeviceAccessKind, DeviceHandle, DeviceId};
pub use handle::{
    DeviceHandleRights, FileHandleRights, HandleOwner, HandleRights, HandleToken,
    SharedRegionRights, SocketHandleRights,
};
pub use identity::{CapabilityEpochs, ObjectIdentity, ObjectKind, ObjectOwner};
pub use session::ConsoleSessionHandle;
