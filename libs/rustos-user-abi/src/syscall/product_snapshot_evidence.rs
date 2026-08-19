/// Seals the exact DVM-backed executable identity into a kernel-stamped
/// product evidence frame. Only the live vfsd endpoint may issue this call.
pub const SYS_RUSTOS_PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE: u64 = 0x5255_004c;
pub const PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE_ABI_VERSION: u16 = 1;
pub const PRODUCT_EXECUTABLE_SNAPSHOT_BACKING_DVM_VOLUME: u16 = 1;

/// Immutable identity supplied by Vfsd after terminally sealing DVM bytes.
/// Ring0 supplies the service identity and endpoint generation itself.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductExecutableSnapshotEvidence {
    pub abi_version: u16,
    pub backing: u16,
    pub flags: u32,
    pub storage_epoch: u64,
    pub mount_generation: u64,
    pub request_id: u64,
    pub file_bytes: u64,
    pub digest: [u8; 32],
    pub reserved0: u64,
    pub reserved1: u64,
}

impl Default for ProductExecutableSnapshotEvidence {
    fn default() -> Self {
        Self {
            abi_version: PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE_ABI_VERSION,
            backing: PRODUCT_EXECUTABLE_SNAPSHOT_BACKING_DVM_VOLUME,
            flags: 0,
            storage_epoch: 0,
            mount_generation: 0,
            request_id: 0,
            file_bytes: 0,
            digest: [0; 32],
            reserved0: 0,
            reserved1: 0,
        }
    }
}

/// Checks the caller-controlled half before endpoint ownership is sampled.
pub const fn product_executable_snapshot_evidence_shape_valid(
    evidence: &ProductExecutableSnapshotEvidence,
) -> bool {
    let mut digest_nonzero = false;
    let mut index = 0;
    while index < evidence.digest.len() {
        digest_nonzero |= evidence.digest[index] != 0;
        index += 1;
    }
    evidence.abi_version == PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE_ABI_VERSION
        && evidence.backing == PRODUCT_EXECUTABLE_SNAPSHOT_BACKING_DVM_VOLUME
        && evidence.flags == 0
        && evidence.storage_epoch != 0
        && evidence.mount_generation != 0
        && evidence.request_id != 0
        && evidence.file_bytes != 0
        && digest_nonzero
        && evidence.reserved0 == 0
        && evidence.reserved1 == 0
}
