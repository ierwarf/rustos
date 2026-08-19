/// Kernel-stamped identity for a DVM-backed executable acceptance witness.
#[derive(Clone, Copy)]
pub struct ProductExecutableSnapshotEvidence {
    pub provider_service_id: u64,
    pub provider_generation: u64,
    pub storage_epoch: u64,
    pub mount_generation: u64,
    pub request_id: u64,
    pub digest: [u8; 32],
}
