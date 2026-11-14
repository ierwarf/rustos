//! Immutable external-pager frame-source control. These tokens are reusable
//! sources, not the one-shot zeroed fault grants and never physical addresses.

/// 0x5255_0061/0062 belong to the app DMA-BUF broker; the two collided until
/// the syscall-number uniqueness test below was added.
pub const SYS_RUSTOS_FRAME_SOURCE: u64 = 0x5255_0063;
pub const FRAME_SOURCE_ABI_VERSION: u16 = 1;
/// Mechanical source-capability envelope, not a cache replacement policy.
pub const FRAME_SOURCE_MAX_LIVE: usize = 16_384;
pub const FRAME_SOURCE_PAGE_BYTES: u64 = 4096;
pub const FRAME_SOURCE_OP_SUPPLY: u16 = 1;
pub const FRAME_SOURCE_OP_RETIRE: u16 = 2;
/// Returns the caller's current published pager service lease epoch. It is
/// useful for pager policy state, but is not an authority token by itself.
pub const FRAME_SOURCE_OP_OWNER_EPOCH: u16 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameSourceKeyWire {
    pub namespace: u64,
    pub object: u64,
    pub object_generation: u64,
    pub content_generation: u64,
    pub page_offset: u64,
}
impl FrameSourceKeyWire {
    pub const fn is_canonical(self) -> bool {
        self.namespace != 0
            && self.object != 0
            && self.object_generation != 0
            && self.content_generation != 0
            && self.page_offset & 4095 == 0
    }
    pub const fn is_empty(self) -> bool {
        self.namespace == 0
            && self.object == 0
            && self.object_generation == 0
            && self.content_generation == 0
            && self.page_offset == 0
    }
}

/// SUPPLY copies `payload_len <= 4096` bytes once and zero-fills the tail. It
/// returns a source token. `rights` authorizes READ and optional EXECUTE of
/// shared source bytes, never WRITE. A writable private view copies on write.
/// Owner service identity and charge group are derived
/// by the kernel, not supplied here. A zero-length source has a null pointer.
/// RETIRE withdraws `token`; all other fields must be zero. Existing installed
/// mappings retain independent frame custody. OWNER_EPOCH has no payload.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosFrameSourceArgs {
    pub abi_version: u16,
    pub op: u16,
    pub flags: u32,
    pub token: u64,
    pub key: FrameSourceKeyWire,
    pub rights: u64,
    pub payload_ptr: u64,
    pub payload_len: u64,
    pub reserved0: u64,
}
impl RustosFrameSourceArgs {
    pub const fn is_canonical(self) -> bool {
        if self.abi_version != FRAME_SOURCE_ABI_VERSION || self.flags != 0 || self.reserved0 != 0 {
            return false;
        }
        match self.op {
            FRAME_SOURCE_OP_SUPPLY => {
                self.token == 0
                    && self.key.is_canonical()
                    && self.rights & crate::pager::VM_PROT_READ as u64 != 0
                    && self.rights & !(crate::pager::VM_PROT_KNOWN as u64) == 0
                    && self.rights & crate::pager::VM_PROT_WRITE as u64 == 0
                    && self.payload_len <= FRAME_SOURCE_PAGE_BYTES
                    && ((self.payload_len == 0 && self.payload_ptr == 0)
                        || (self.payload_len != 0 && self.payload_ptr != 0))
            }
            FRAME_SOURCE_OP_RETIRE => {
                self.token != 0
                    && self.key.is_empty()
                    && self.rights == 0
                    && self.payload_ptr == 0
                    && self.payload_len == 0
            }
            FRAME_SOURCE_OP_OWNER_EPOCH => {
                self.token == 0
                    && self.key.is_empty()
                    && self.rights == 0
                    && self.payload_ptr == 0
                    && self.payload_len == 0
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn supply() -> RustosFrameSourceArgs {
        RustosFrameSourceArgs {
            abi_version: 1,
            op: FRAME_SOURCE_OP_SUPPLY,
            key: FrameSourceKeyWire {
                namespace: 2,
                object: 3,
                object_generation: 4,
                content_generation: 5,
                page_offset: 0,
            },
            rights: 5,
            payload_ptr: 0x8000,
            payload_len: 4096,
            ..Default::default()
        }
    }
    #[test]
    fn supply_canonicality_is_exact_and_has_no_owner_or_charge_input() {
        assert_eq!(core::mem::size_of::<RustosFrameSourceArgs>(), 88);
        let good = supply();
        assert!(good.is_canonical());
        for bad in [
            RustosFrameSourceArgs {
                abi_version: 0,
                ..good
            },
            RustosFrameSourceArgs { flags: 1, ..good },
            RustosFrameSourceArgs {
                reserved0: 1,
                ..good
            },
            RustosFrameSourceArgs { token: 1, ..good },
            RustosFrameSourceArgs {
                payload_len: 4097,
                ..good
            },
            RustosFrameSourceArgs {
                payload_ptr: 0,
                ..good
            },
            RustosFrameSourceArgs { rights: 3, ..good },
            RustosFrameSourceArgs { rights: 6, ..good },
            RustosFrameSourceArgs { rights: 8, ..good },
            RustosFrameSourceArgs {
                rights: 1 << 63,
                ..good
            },
            RustosFrameSourceArgs {
                key: FrameSourceKeyWire {
                    page_offset: 1,
                    ..good.key
                },
                ..good
            },
        ] {
            assert!(!bad.is_canonical());
        }
        assert!(
            RustosFrameSourceArgs {
                payload_ptr: 0,
                payload_len: 0,
                ..good
            }
            .is_canonical()
        );
        assert!(
            !RustosFrameSourceArgs {
                payload_len: 0,
                ..good
            }
            .is_canonical()
        );
    }
    #[test]
    fn retirement_and_epoch_query_carry_no_hidden_supply_authority() {
        let retire = RustosFrameSourceArgs {
            abi_version: 1,
            op: FRAME_SOURCE_OP_RETIRE,
            token: 7,
            ..Default::default()
        };
        assert!(retire.is_canonical());
        assert!(
            !RustosFrameSourceArgs {
                payload_ptr: 1,
                ..retire
            }
            .is_canonical()
        );
        assert!(
            !RustosFrameSourceArgs {
                key: supply().key,
                ..retire
            }
            .is_canonical()
        );
        let query = RustosFrameSourceArgs {
            abi_version: 1,
            op: FRAME_SOURCE_OP_OWNER_EPOCH,
            ..Default::default()
        };
        assert!(query.is_canonical());
        assert!(!RustosFrameSourceArgs { token: 1, ..query }.is_canonical());
        assert_ne!(
            SYS_RUSTOS_FRAME_SOURCE,
            crate::syscall::SYS_RUSTOS_PAGER_FAULT_REPLY
        );
    }
}
