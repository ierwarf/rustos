#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAbi {
    Linux,
    Windows,
}

impl UserAbi {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}
