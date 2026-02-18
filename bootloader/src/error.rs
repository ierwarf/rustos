use uefi::prelude::*;

#[derive(Debug, Clone, Copy)]
pub enum BootError {
    OpenFileSystem(Status),
    ReadKernel(Status),
    InvalidElf(&'static str),
    SegmentAlloc(Status),
}

impl BootError {
    pub const fn status(self) -> Status {
        match self {
            Self::OpenFileSystem(status) | Self::ReadKernel(status) | Self::SegmentAlloc(status) => {
                status
            }
            Self::InvalidElf(_) => Status::LOAD_ERROR,
        }
    }
}
