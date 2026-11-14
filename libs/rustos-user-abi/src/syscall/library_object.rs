//! Shared immutable-file cache objects (`pagerd` policy, ring0 mechanism).
//!
//! A library object is a set of immutable, source-pinned frames that together
//! hold one whole file of an immutable mount. Its authority is the vfsd
//! `VfsFileIdentityWire` (volume object + generations + length) plus the content
//! hash `pagerd` verified while loading it. A path or basename is never an
//! identity. `pagerd` creates, fills, seals and retires objects; ring0 only
//! records them, serves sealed objects to guest `mmap` of an exactly matching
//! open description, and withholds everything else (the eager copy path remains).

pub const SYS_RUSTOS_LIBRARY_OBJECT: u64 = 0x5255_0064;
pub const LIBRARY_OBJECT_ABI_VERSION: u16 = 1;
/// Mechanical registry envelope; which libraries are worth caching is pagerd
/// policy (`rustos-library-cache-policy`), not a ring0 decision.
pub const LIBRARY_OBJECT_MAX_OBJECTS: usize = 128;
/// 4 MiB, the policy's per-file ceiling. Larger files stay on the lazy path.
pub const LIBRARY_OBJECT_MAX_PAGES: u64 = 1024;
/// Backing namespace stamped into every VMA that maps a library object.
pub const LIBRARY_OBJECT_BACKING_NAMESPACE: u64 = crate::syscall::IPC_SERVICE_PAGERD;

/// CREATE registers an identity and reserves `identity.len` bytes of pages.
/// Returns the object handle (`generation << 16 | slot`).
pub const LIBRARY_OBJECT_OP_CREATE: u16 = 1;
/// SUPPLY_PAGE copies `payload_len <= 4096` bytes into page `page_index` of an
/// unsealed object and returns that page's frame-source token. The tail of a
/// short (last) page is zero. Pages may be supplied exactly once.
pub const LIBRARY_OBJECT_OP_SUPPLY_PAGE: u16 = 2;
/// SEAL publishes an object whose every page has been supplied. Only a sealed,
/// live object can be bound to an open file.
pub const LIBRARY_OBJECT_OP_SEAL: u16 = 3;
/// RETIRE withdraws the object and every page source. Existing private views
/// keep their independently owned frames.
pub const LIBRARY_OBJECT_OP_RETIRE: u16 = 4;

/// PRESSURE reads the allocator's free/reserve counters into the
/// `LibraryPressureWire` at `payload_ptr`. Read-only; it names no object and
/// lets the pager (which owns what to retire) see why to retire something.
pub const LIBRARY_OBJECT_OP_PRESSURE: u16 = 5;

/// vfsd reports this for files that live on the read-only DVM volume mount.
pub const VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME: u64 = 1 << 0;
/// `VfsFileIdentityWire::volume` of every file served by the authenticated
/// early-system image. It is a reserved id, not a storage volume: the image is
/// read-only for the whole boot, its `object` is the SHA-256 prefix of the
/// image-relative path, and its epoch/mount generation are fixed at 1.
pub const VFS_EARLY_SYSTEM_VOLUME_ID: u64 = 0x4553_5953_494d_4701;

pub const VFS_FILE_IDENTITY_FLAGS_KNOWN: u64 = VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME;

/// vfsd-attested identity of one open file description's backing file. The
/// `volume`/`object`/`generation` triple is the storage layer's file identity;
/// `storage_epoch` and `mount_generation` end its validity at a remount or
/// provider restart.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfsFileIdentityWire {
    pub volume: u64,
    pub object: u64,
    pub generation: u64,
    pub storage_epoch: u64,
    pub mount_generation: u64,
    pub len: u64,
    pub flags: u64,
}
impl VfsFileIdentityWire {
    pub const fn is_canonical(self) -> bool {
        self.object != 0
            && self.generation != 0
            && self.storage_epoch != 0
            && self.mount_generation != 0
            && self.len != 0
            && self.flags & !VFS_FILE_IDENTITY_FLAGS_KNOWN == 0
    }
    /// Only files on the immutable volume are cacheable.
    pub const fn is_immutable(self) -> bool {
        self.flags & VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME != 0
    }
    /// Fixed little-endian payload carried by `VFS_IPC_OP_FILE_IDENTITY`.
    pub const WIRE_BYTES: usize = 56;
    pub fn to_bytes(self) -> [u8; Self::WIRE_BYTES] {
        let mut out = [0_u8; Self::WIRE_BYTES];
        for (index, value) in [
            self.volume,
            self.object,
            self.generation,
            self.storage_epoch,
            self.mount_generation,
            self.len,
            self.flags,
        ]
        .into_iter()
        .enumerate()
        {
            out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        out
    }
    /// Exactly one payload; any other length is not an identity.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::WIRE_BYTES {
            return None;
        }
        let field = |index: usize| {
            let mut value = [0_u8; 8];
            value.copy_from_slice(&bytes[index * 8..index * 8 + 8]);
            u64::from_le_bytes(value)
        };
        Some(Self {
            volume: field(0),
            object: field(1),
            generation: field(2),
            storage_epoch: field(3),
            mount_generation: field(4),
            len: field(5),
            flags: field(6),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LibraryIdentityWire {
    pub file: VfsFileIdentityWire,
    /// SHA-256 pagerd verified over the whole file while loading it.
    pub content_hash: [u8; 32],
}
impl LibraryIdentityWire {
    pub fn is_canonical(&self) -> bool {
        self.file.is_canonical()
            && self.file.is_immutable()
            && self.file.len <= LIBRARY_OBJECT_MAX_PAGES * 4096
            && self.content_hash != [0; 32]
    }
    pub const fn page_count(&self) -> u64 {
        self.file.len.div_ceil(4096)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosLibraryObjectArgs {
    pub abi_version: u16,
    pub op: u16,
    pub flags: u32,
    /// Handle returned by CREATE; zero on CREATE.
    pub object: u64,
    pub page_index: u64,
    pub identity: LibraryIdentityWire,
    pub payload_ptr: u64,
    pub payload_len: u64,
    pub reserved0: u64,
}
impl RustosLibraryObjectArgs {
    pub fn is_canonical(&self) -> bool {
        if self.abi_version != LIBRARY_OBJECT_ABI_VERSION
            || self.flags != 0
            || self.reserved0 != 0
        {
            return false;
        }
        let no_identity = self.identity == LibraryIdentityWire::default();
        let no_payload = self.payload_ptr == 0 && self.payload_len == 0;
        match self.op {
            LIBRARY_OBJECT_OP_CREATE => {
                self.object == 0
                    && self.page_index == 0
                    && no_payload
                    && self.identity.is_canonical()
            }
            LIBRARY_OBJECT_OP_SUPPLY_PAGE => {
                self.object != 0
                    && self.page_index < LIBRARY_OBJECT_MAX_PAGES
                    && no_identity
                    && self.payload_len != 0
                    && self.payload_len <= 4096
                    && self.payload_ptr != 0
            }
            LIBRARY_OBJECT_OP_SEAL | LIBRARY_OBJECT_OP_RETIRE => {
                self.object != 0 && self.page_index == 0 && no_identity && no_payload
            }
            LIBRARY_OBJECT_OP_PRESSURE => {
                self.object == 0
                    && self.page_index == 0
                    && no_identity
                    && self.payload_ptr != 0
                    && self.payload_len == core::mem::size_of::<LibraryPressureWire>() as u64
            }
            _ => false,
        }
    }
}

crate::user_wire!(VfsFileIdentityWire {
    volume: u64,
    object: u64,
    generation: u64,
    storage_epoch: u64,
    mount_generation: u64,
    len: u64,
    flags: u64
});

/// Allocator counters, in pages, for the pager's retention policy. The kernel
/// publishes numbers only; whether any is pressure is the pager's decision.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LibraryPressureWire {
    pub free_pages: u64,
    pub min_free_pages: u64,
    pub watermark_denials: u64,
    /// Pages currently reserved by registry objects.
    pub reserved_pages: u64,
}
crate::user_wire!(LibraryPressureWire {
    free_pages: u64,
    min_free_pages: u64,
    watermark_denials: u64,
    reserved_pages: u64
});

/// `MM_BROKER_OP_BIND_LIBRARY` result. `bound` is nonzero only when the open
/// description now carries a verified binding to a sealed library object.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosMmLibraryBindBrokerResult {
    pub bound: u64,
    pub object: u64,
    pub len: u64,
}
crate::user_wire!(RustosMmLibraryBindBrokerResult { bound: u64, object: u64, len: u64 });

/// Read-only diagnostic of the calling process's own page tables, reachable
/// only from the exact probe executable `LIBRARY_MAP_PROBE_EXEC_PATH` (the
/// kernel checks the caller's exec path). It exists to prove frame sharing and
/// release at runtime; it grants nothing and mutates nothing.
pub const SYS_RUSTOS_LIBRARY_MAP_PROBE: u64 = 0x5255_0065;
pub const LIBRARY_MAP_PROBE_EXEC_PATH: &str = "/apps/libmapprobe/libmapprobe.elf";
/// FRAMES reports, per page of `[addr, addr + count * 4096)`, the physical
/// frame behind this process's leaf (zero if none) and its permission bits.
pub const LIBRARY_MAP_PROBE_OP_FRAMES: u32 = 1;
/// STATS reports global free pages, this process's data-leaf count and the
/// library registry's counters.
pub const LIBRARY_MAP_PROBE_OP_STATS: u32 = 2;
pub const LIBRARY_MAP_PROBE_MAX_PAGES: u32 = 64;
pub const LIBRARY_MAP_PROBE_FRAME_PRESENT: u64 = 1 << 0;
pub const LIBRARY_MAP_PROBE_FRAME_WRITABLE: u64 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosLibraryMapProbeArgs {
    pub op: u32,
    pub count: u32,
    pub addr: u64,
    pub out_ptr: u64,
    /// FRAMES target: zero for the caller, else another live process running
    /// the same probe executable (a sibling or child of the probe).
    pub pid: u64,
}
crate::user_wire!(RustosLibraryMapProbeArgs { op: u32, count: u32, addr: u64, out_ptr: u64, pid: u64 });

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LibraryMapProbeFrame {
    pub phys: u64,
    pub flags: u64,
}
crate::user_wire!(LibraryMapProbeFrame { phys: u64, flags: u64 });

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LibraryMapProbeStats {
    pub free_pages: u64,
    pub own_data_leaves: u64,
    pub sealed_objects: u64,
    pub reserved_pages: u64,
    pub alias_maps: u64,
    pub alias_pages: u64,
    /// Per `Decline` reason: how many mappings were not served, and the latest
    /// detail (errno, length, ...).
    pub decline_counts: [u64; 8],
    pub decline_details: [u64; 8],
}
crate::user_wire!(LibraryMapProbeStats {
    free_pages: u64,
    own_data_leaves: u64,
    sealed_objects: u64,
    reserved_pages: u64,
    alias_maps: u64,
    alias_pages: u64,
    decline_counts: [u64; 8],
    decline_details: [u64; 8]
});

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> LibraryIdentityWire {
        LibraryIdentityWire {
            file: VfsFileIdentityWire {
                volume: 1,
                object: 9,
                generation: 3,
                storage_epoch: 5,
                mount_generation: 2,
                len: 5000,
                flags: VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME,
            },
            content_hash: [7; 32],
        }
    }
    fn create() -> RustosLibraryObjectArgs {
        RustosLibraryObjectArgs {
            abi_version: LIBRARY_OBJECT_ABI_VERSION,
            op: LIBRARY_OBJECT_OP_CREATE,
            identity: identity(),
            ..Default::default()
        }
    }

    #[test]
    fn layouts_are_fixed() {
        assert_eq!(core::mem::size_of::<VfsFileIdentityWire>(), 56);
        assert_eq!(core::mem::size_of::<LibraryIdentityWire>(), 88);
        assert_eq!(core::mem::size_of::<RustosLibraryObjectArgs>(), 8 + 8 + 8 + 88 + 24);
        assert!(core::mem::size_of::<VfsFileIdentityWire>() <= super::super::VFS_IPC_PAYLOAD_CAPACITY);
        assert_eq!(identity().page_count(), 2);
        assert_eq!(VfsFileIdentityWire::WIRE_BYTES, core::mem::size_of::<VfsFileIdentityWire>());
    }

    #[test]
    fn pressure_request_is_read_only_and_exactly_sized() {
        let request = RustosLibraryObjectArgs {
            abi_version: LIBRARY_OBJECT_ABI_VERSION,
            op: LIBRARY_OBJECT_OP_PRESSURE,
            payload_ptr: 0x1000,
            payload_len: core::mem::size_of::<LibraryPressureWire>() as u64,
            ..Default::default()
        };
        assert!(request.is_canonical());
        assert_eq!(core::mem::size_of::<LibraryPressureWire>(), 32);
        for bad in [
            RustosLibraryObjectArgs { payload_ptr: 0, ..request },
            RustosLibraryObjectArgs { payload_len: 31, ..request },
            RustosLibraryObjectArgs { object: 1, ..request },
            RustosLibraryObjectArgs { page_index: 1, ..request },
            RustosLibraryObjectArgs { identity: identity(), ..request },
        ] {
            assert!(!bad.is_canonical());
        }
    }

    #[test]
    fn identity_payload_round_trips_and_rejects_any_other_length() {
        let wire = identity().file;
        let bytes = wire.to_bytes();
        assert_eq!(VfsFileIdentityWire::from_bytes(&bytes), Some(wire));
        assert_eq!(VfsFileIdentityWire::from_bytes(&bytes[..55]), None);
        let mut long = [0_u8; 57];
        long[..56].copy_from_slice(&bytes);
        assert_eq!(VfsFileIdentityWire::from_bytes(&long), None);
    }

    #[test]
    fn identity_requires_immutable_mount_hash_and_bounded_length() {
        assert!(identity().is_canonical());
        let mut bad = identity();
        bad.file.flags = 0;
        assert!(!bad.is_canonical(), "mutable mount files are never cacheable");
        let mut bad = identity();
        bad.content_hash = [0; 32];
        assert!(!bad.is_canonical());
        let mut bad = identity();
        bad.file.len = LIBRARY_OBJECT_MAX_PAGES * 4096 + 1;
        assert!(!bad.is_canonical());
        let mut bad = identity();
        bad.file.len = 0;
        assert!(!bad.is_canonical());
        let mut bad = identity();
        bad.file.flags |= 1 << 5;
        assert!(!bad.is_canonical());
        for field in 0..5 {
            let mut bad = identity();
            match field {
                0 => bad.file.object = 0,
                1 => bad.file.generation = 0,
                2 => bad.file.storage_epoch = 0,
                3 => bad.file.mount_generation = 0,
                _ => bad.file.len = 0,
            }
            assert!(!bad.is_canonical());
        }
    }

    #[test]
    fn operations_carry_exactly_their_own_fields() {
        assert!(create().is_canonical());
        assert!(!RustosLibraryObjectArgs { object: 1, ..create() }.is_canonical());
        assert!(!RustosLibraryObjectArgs { payload_len: 1, payload_ptr: 1, ..create() }.is_canonical());
        assert!(!RustosLibraryObjectArgs { flags: 1, ..create() }.is_canonical());
        let supply = RustosLibraryObjectArgs {
            abi_version: LIBRARY_OBJECT_ABI_VERSION,
            op: LIBRARY_OBJECT_OP_SUPPLY_PAGE,
            object: 4,
            page_index: 1,
            payload_ptr: 0x1000,
            payload_len: 904,
            ..Default::default()
        };
        assert!(supply.is_canonical());
        for bad in [
            RustosLibraryObjectArgs { object: 0, ..supply },
            RustosLibraryObjectArgs { payload_len: 0, ..supply },
            RustosLibraryObjectArgs { payload_len: 4097, ..supply },
            RustosLibraryObjectArgs { payload_ptr: 0, ..supply },
            RustosLibraryObjectArgs { page_index: LIBRARY_OBJECT_MAX_PAGES, ..supply },
            RustosLibraryObjectArgs { identity: identity(), ..supply },
        ] {
            assert!(!bad.is_canonical());
        }
        for op in [LIBRARY_OBJECT_OP_SEAL, LIBRARY_OBJECT_OP_RETIRE] {
            let control = RustosLibraryObjectArgs {
                abi_version: LIBRARY_OBJECT_ABI_VERSION,
                op,
                object: 4,
                ..Default::default()
            };
            assert!(control.is_canonical());
            assert!(!RustosLibraryObjectArgs { page_index: 1, ..control }.is_canonical());
            assert!(!RustosLibraryObjectArgs { payload_len: 1, payload_ptr: 1, ..control }.is_canonical());
            assert!(!RustosLibraryObjectArgs { object: 0, ..control }.is_canonical());
        }
        assert!(!RustosLibraryObjectArgs { op: 99, ..create() }.is_canonical());
    }
}
