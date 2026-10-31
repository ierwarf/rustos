//! Deterministic, fail-closed compilation of signed component policy inputs.
//!
//! Validation does not verify a signature. The caller must first authenticate
//! an immutable manifest using RustOS's existing signed-image chain. A valid
//! component graph is then constructed transactionally, without publishing
//! any kernel descriptor or service endpoint.

use alloc::vec::Vec;

use crate::identity::{ComponentId, Moniker, MonikerError};
use crate::policy::JobCeiling;
use crate::quota::Quota;
use crate::realm::{Graph, GraphError};
use crate::supervision::RestartPolicy;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestError {
    Empty,
    TooManyComponents,
    InvalidName,
    InvalidDependency,
    DuplicateJob,
    MissingRoot,
    InvalidRoute,
    InvalidManifest,
    OutOfMemory,
}

#[derive(Clone, Copy)]
pub struct ExportSpec<'a> {
    pub name: &'a str,
    pub rights: u32,
}

#[derive(Clone, Copy)]
pub struct OfferSpec<'a> {
    pub provider_index: usize,
    pub consumer_index: usize,
    pub name: &'a str,
    pub rights: u32,
}

#[derive(Clone, Copy)]
pub struct ComponentSpec<'a> {
    /// Parent is an earlier index in the same manifest. Index 0 is the root.
    pub parent_index: usize,
    pub name: &'a str,
    /// Kernel JobId observed by rootd from an independently checked descriptor.
    pub kernel_job_id: u64,
    pub ceiling: JobCeiling,
    pub quota: Quota,
    pub restart: RestartPolicy,
    pub critical: bool,
    pub exports: &'a [ExportSpec<'a>],
}

impl Graph {
    /// Creates a complete policy graph or returns an error without publishing
    /// anything. Kernel Job creation and endpoint binding are separate,
    /// fallible transactions executed by the supervisor, not by this library.
    pub fn from_manifest(
        root_name: &str,
        root_job: u64,
        specs: &[ComponentSpec<'_>],
        offers: &[OfferSpec<'_>],
    ) -> Result<Self, ManifestError> {
        if specs.is_empty() {
            return Err(ManifestError::Empty);
        }
        if specs.len() > crate::realm::MAX_COMPONENTS {
            return Err(ManifestError::TooManyComponents);
        }
        if specs.len() > crate::realm::MAX_ROUTE_RECORDS
            || offers.len() > crate::realm::MAX_ROUTE_RECORDS
        {
            return Err(ManifestError::TooManyComponents);
        }
        let root = Moniker::new(root_name).map_err(|_| ManifestError::InvalidName)?;
        let mut graph = Graph::new(root, root_job, specs.len()).map_err(map_graph_error)?;
        let mut ids: Vec<ComponentId> = Vec::new();
        ids.try_reserve_exact(specs.len())
            .map_err(|_| ManifestError::OutOfMemory)?;
        ids.push(graph.root());

        // First descriptor describes the root. It cannot silently redefine its
        // Job identity, and root's restart policy is always NEVER.
        let first = &specs[0];
        if first.parent_index != 0
            || first.kernel_job_id != root_job
            || first.name != root_name
            || first.ceiling != JobCeiling::ALL
            || first.quota != Quota::UNLIMITED
            || first.restart != RestartPolicy::NEVER
            || !first.critical
        {
            return Err(ManifestError::InvalidManifest);
        }

        for (index, spec) in specs.iter().enumerate().skip(1) {
            if spec.parent_index >= index {
                return Err(ManifestError::InvalidDependency);
            }
            if specs[..index]
                .iter()
                .any(|older| older.kernel_job_id == spec.kernel_job_id)
            {
                return Err(ManifestError::DuplicateJob);
            }
            let name = Moniker::new(spec.name).map_err(|_| ManifestError::InvalidName)?;
            let parent = ids[spec.parent_index];
            let id = graph
                .create(
                    parent,
                    name,
                    spec.kernel_job_id,
                    spec.ceiling,
                    spec.quota,
                    spec.restart,
                    spec.critical,
                )
                .map_err(map_graph_error)?;
            ids.push(id);
        }
        for id in ids.iter().copied().skip(1) {
            graph.activate(id).map_err(map_graph_error)?;
        }

        for (index, spec) in specs.iter().enumerate() {
            for export in spec.exports {
                let name = Moniker::new(export.name).map_err(|_| ManifestError::InvalidName)?;
                graph
                    .publish(ids[index], name, export.rights)
                    .map_err(|_| ManifestError::InvalidRoute)?;
            }
        }
        for offer in offers {
            let provider = *ids
                .get(offer.provider_index)
                .ok_or(ManifestError::InvalidRoute)?;
            let consumer = *ids
                .get(offer.consumer_index)
                .ok_or(ManifestError::InvalidRoute)?;
            let name = Moniker::new(offer.name).map_err(|_| ManifestError::InvalidName)?;
            graph
                .offer(provider, consumer, name, offer.rights)
                .map_err(|_| ManifestError::InvalidRoute)?;
        }
        Ok(graph)
    }
}

fn map_graph_error(err: GraphError) -> ManifestError {
    match err {
        GraphError::OutOfMemory => ManifestError::OutOfMemory,
        GraphError::DuplicateName => ManifestError::InvalidManifest,
        GraphError::InvalidParent => ManifestError::InvalidDependency,
        GraphError::InvalidJob => ManifestError::DuplicateJob,
        GraphError::Full => ManifestError::TooManyComponents,
        _ => ManifestError::InvalidManifest,
    }
}

impl From<MonikerError> for ManifestError {
    fn from(_: MonikerError) -> Self {
        ManifestError::InvalidName
    }
}
