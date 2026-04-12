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
