//! Evidence-profile admission, including the finite pre-QEMU TLC budget.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;

use super::registry::ModelRecord;

#[derive(Debug, Deserialize)]
pub(crate) struct EvidenceProfile {
    pub evidence_max_age_hours: u64,
    /// Fail-closed wall budget for the selected TLC profile, not a request to
    /// silently reduce any individual model's finite configuration.
    pub tlc_max_wall_seconds: u64,
    /// Exact TLC pass evidence may be reused for at most this many hours.
    /// Zero disables reuse for qualification profiles.
    pub tlc_reuse_max_age_hours: u64,
    #[serde(default)]
    pub required_models: Vec<String>,
    pub required_evidence: Vec<PathBuf>,
}

impl EvidenceProfile {
    pub(crate) fn validate(&self, models: &BTreeMap<String, ModelRecord>) -> Result<()> {
        if self.evidence_max_age_hours == 0 {
            bail!("evidence profile max age must be positive");
        }
        if self.tlc_max_wall_seconds == 0 {
            bail!("evidence profile TLC wall budget must be positive");
        }
        if self.tlc_reuse_max_age_hours > self.evidence_max_age_hours {
            bail!("TLC reuse age cannot exceed the profile evidence age");
        }
        let mut profile_models = BTreeSet::new();
        for model in &self.required_models {
            if !models.contains_key(model) {
                bail!("evidence profile uses unknown model {model}");
            }
            if !profile_models.insert(model) {
                bail!("evidence profile repeats model {model}");
            }
        }
        let mut evidence_paths = BTreeSet::new();
        for evidence in &self.required_evidence {
            let valid = evidence.is_relative()
                && evidence
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)));
            if !valid {
                bail!(
                    "profile evidence must be a relative normal path: {}",
                    evidence.display()
                );
            }
            if !evidence_paths.insert(evidence) {
                bail!("evidence profile repeats {}", evidence.display());
            }
        }
        Ok(())
    }
}
