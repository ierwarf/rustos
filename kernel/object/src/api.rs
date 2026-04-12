pub mod device {
    pub use crate::device::{DeviceAccessKind, DeviceHandle, DeviceId};
}

pub mod handle {
    pub use crate::handle::{HandleOwner, HandleToken};
}

pub mod session {
    pub use crate::session::ConsoleSessionHandle;
}

pub use device::{DeviceAccessKind, DeviceHandle, DeviceId};
pub use handle::{HandleOwner, HandleToken};
pub use session::ConsoleSessionHandle;
