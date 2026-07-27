use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::Result;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

impl Severity {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "critical" => Ok(Self::Critical),
            "high" => Ok(Self::High),
            "medium" => Ok(Self::Medium),
            "low" => Ok(Self::Low),
            _ => bail!("invalid formal severity: {value}"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Outcome {
    Continue,
    Success,
    Error,
    Timeout,
    Cancel,
    Revoke,
    Exit,
}

impl Outcome {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "continue" => Ok(Self::Continue),
            "success" => Ok(Self::Success),
            "error" => Ok(Self::Error),
            "timeout" => Ok(Self::Timeout),
            "cancel" => Ok(Self::Cancel),
            "revoke" => Ok(Self::Revoke),
            "exit" => Ok(Self::Exit),
            _ => bail!("invalid formal outcome: {value}"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Success => "success",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Cancel => "cancel",
            Self::Revoke => "revoke",
            Self::Exit => "exit",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ModelRecord {
    pub name: String,
    pub class: String,
    pub deadlock_policy: String,
    pub deadlock_reason: String,
    pub pr_timeout_s: u64,
    pub nightly_timeout_s: u64,
    pub nightly_mode: String,
    pub apalache: bool,
    pub tlaps: bool,
    pub trace: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct WitnessRecord {
    pub model: String,
    pub package: String,
    pub test: String,
    pub features: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Transition {
    pub flow: String,
    pub id: String,
    pub requirement: String,
    pub hazard: String,
    pub severity: Severity,
    pub owner: String,
    pub from: String,
    pub event: String,
    pub to: String,
    pub outcome: Outcome,
    pub max_wait_ms: u64,
    pub model: String,
    pub source: PathBuf,
    pub witness_package: String,
    pub witness_test: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContractManifest {
    pub schema: u32,
    pub max_intentional_terminal_models: usize,
    pub models: PathBuf,
    pub model_bindings: PathBuf,
    pub flows: PathBuf,
    pub scenarios: PathBuf,
    pub witnesses: PathBuf,
    pub generated_doc: PathBuf,
    #[serde(default)]
    pub source_mappings: Vec<SourceMapping>,
    #[serde(default)]
    pub risk_surfaces: Vec<RiskSurface>,
    pub profiles: BTreeMap<String, EvidenceProfile>,
    pub topologies: BTreeMap<String, EvidenceTopology>,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelBinding {
    pub model: String,
    pub role: String,
    pub flow: String,
    pub rationale: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SourceMapping {
    pub path: PathBuf,
    pub models: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RiskSurface {
    pub path: PathBuf,
    pub severity: String,
    pub flows: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvidenceProfile {
    pub evidence_max_age_hours: u64,
    pub required_evidence: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvidenceTopology {
    pub runtime_scenario: String,
    pub hard_deadline_ms: u64,
    pub required_runtime_models: Vec<String>,
    pub required_artifacts: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProductScenarioStep {
    pub scenario: String,
    pub topology: String,
    pub sequence: usize,
    pub step: String,
    pub flow: String,
    pub transition: String,
    pub model: String,
    pub log: String,
    pub marker: String,
    pub requires: Vec<String>,
    pub deadline_ms: u64,
}

#[derive(Debug)]
pub(crate) struct ContractRegistry {
    pub manifest: ContractManifest,
    pub models: BTreeMap<String, ModelRecord>,
    pub model_bindings: Vec<ModelBinding>,
    pub transitions: Vec<Transition>,
    pub scenarios: Vec<ProductScenarioStep>,
    pub witnesses: BTreeSet<WitnessRecord>,
}

#[derive(Debug, Default)]
pub(crate) struct ContractImpact {
    pub models: BTreeSet<String>,
    pub witnesses: BTreeSet<WitnessRecord>,
    pub unmapped_high_risk: BTreeSet<PathBuf>,
}

#[derive(Debug)]
struct FlowGraph<'a> {
    transitions: Vec<&'a Transition>,
    nodes: BTreeSet<&'a str>,
    outgoing: BTreeMap<&'a str, BTreeSet<&'a str>>,
    incoming: BTreeMap<&'a str, BTreeSet<&'a str>>,
}

impl ContractRegistry {
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let manifest_path = root.join("formal/contracts.toml");
        let manifest_text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?;
        let manifest: ContractManifest =
            toml::from_str(&manifest_text).context("parse formal/contracts.toml")?;
        if manifest.schema != 3 {
            bail!("unsupported formal contract schema {}", manifest.schema);
        }
        let models = parse_models(&root.join(&manifest.models))?;
        let model_bindings = parse_model_bindings(&root.join(&manifest.model_bindings))?;
        let transitions = parse_transitions(&root.join(&manifest.flows))?;
        let scenarios = parse_product_scenarios(&root.join(&manifest.scenarios))?;
        let witnesses = parse_witnesses(&root.join(&manifest.witnesses))?;
        Ok(Self {
            manifest,
            models,
            model_bindings,
            transitions,
            scenarios,
            witnesses,
        })
    }

    pub(crate) fn flow_count(&self) -> usize {
        self.transitions
            .iter()
            .map(|transition| transition.flow.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }

    pub(crate) fn validate(&self, root: &Path) -> Result<()> {
        let mut transition_ids = BTreeSet::new();
        let mut requirements = BTreeSet::new();
        let mut hazards = BTreeSet::new();
        for transition in &self.transitions {
            if !transition_ids.insert((&transition.flow, &transition.id)) {
                bail!(
                    "duplicate formal transition {}/{}",
                    transition.flow,
                    transition.id
                );
            }
            if !requirements.insert(&transition.requirement) {
                bail!("duplicate formal requirement {}", transition.requirement);
            }
            if !hazards.insert(&transition.hazard) {
                bail!("duplicate formal hazard {}", transition.hazard);
            }
            if !self.models.contains_key(&transition.model) {
                bail!(
                    "transition {}/{} uses unknown model {}",
                    transition.flow,
                    transition.id,
                    transition.model
                );
            }
            if !root.join(&transition.source).is_file() {
                bail!(
                    "transition {}/{} source does not exist: {}",
                    transition.flow,
                    transition.id,
                    transition.source.display()
                );
            }
            let witness = WitnessRecord {
                model: transition.model.clone(),
                package: transition.witness_package.clone(),
                test: transition.witness_test.clone(),
                features: String::new(),
            };
            if !self.witnesses.iter().any(|registered| {
                registered.model == witness.model
                    && registered.package == witness.package
                    && registered.test == witness.test
            }) {
                bail!(
                    "transition {}/{} lacks exact source witness {}|{}|{}",
                    transition.flow,
                    transition.id,
                    witness.model,
                    witness.package,
                    witness.test
                );
            }
            if transition.outcome == Outcome::Timeout && transition.max_wait_ms == 0 {
                bail!(
                    "timeout transition {}/{} has no positive bound",
                    transition.flow,
                    transition.id
                );
            }
        }
        for (flow, graph) in self.flow_graphs() {
            validate_flow_graph(flow, &graph)?;
        }
        let flow_ids = self
            .transitions
            .iter()
            .map(|transition| transition.flow.as_str())
            .collect::<BTreeSet<_>>();
        let mut bound_models = self
            .transitions
            .iter()
            .map(|transition| transition.model.as_str())
            .collect::<BTreeSet<_>>();
        let mut binding_keys = BTreeSet::new();
        for binding in &self.model_bindings {
            if !self.models.contains_key(&binding.model) {
                bail!("model binding uses unknown model {}", binding.model);
            }
            if !matches!(binding.role.as_str(), "refinement" | "evidence") {
                bail!(
                    "model binding {} has invalid role {}",
                    binding.model,
                    binding.role
                );
            }
            if !flow_ids.contains(binding.flow.as_str()) {
                bail!(
                    "model binding {} uses unknown whole flow {}",
                    binding.model,
                    binding.flow
                );
            }
            if binding.rationale.trim().is_empty() {
                bail!("model binding {} lacks a rationale", binding.model);
            }
            if !binding_keys.insert((&binding.model, &binding.flow)) {
                bail!(
                    "duplicate model binding {} -> {}",
                    binding.model,
                    binding.flow
                );
            }
            if binding.role == "refinement"
                && !self
                    .witnesses
                    .iter()
                    .any(|witness| witness.model == binding.model)
            {
                bail!(
                    "refinement model binding {} -> {} lacks an exact source witness",
                    binding.model,
                    binding.flow
                );
            }
            bound_models.insert(binding.model.as_str());
        }
        let orphan_models = self
            .models
            .keys()
            .map(String::as_str)
            .filter(|model| !bound_models.contains(model))
            .collect::<Vec<_>>();
        if !orphan_models.is_empty() {
            bail!(
                "formal models are not bound to a whole flow: {}",
                orphan_models.join(", ")
            );
        }
        let mut scenario_groups: BTreeMap<(&str, &str), Vec<&ProductScenarioStep>> =
            BTreeMap::new();
        for step in &self.scenarios {
            scenario_groups
                .entry((step.scenario.as_str(), step.topology.as_str()))
                .or_default()
                .push(step);
        }
        for ((scenario, topology), steps) in &mut scenario_groups {
            if !self.manifest.topologies.contains_key(*topology) {
                bail!("product scenario {scenario} uses unknown topology {topology}");
            }
            steps.sort_by_key(|step| step.sequence);
            let mut step_names = BTreeSet::new();
            let mut completed: BTreeMap<&str, (&str, u64)> = BTreeMap::new();
            for (expected_sequence, step) in steps.iter().enumerate() {
                if step.sequence != expected_sequence {
                    bail!(
                        "product scenario {scenario}/{topology} sequence is not contiguous at {}",
                        step.step
                    );
                }
                if !step_names.insert(step.step.as_str()) {
                    bail!(
                        "product scenario {scenario}/{topology} repeats step {}",
                        step.step
                    );
                }
                if !matches!(step.log.as_str(), "rustos" | "dvm") {
                    bail!(
                        "product scenario {scenario}/{topology} step {} uses invalid log {}",
                        step.step,
                        step.log
                    );
                }
                if step.marker.is_empty() || step.marker.contains(['\n', '\r', '\t']) {
                    bail!(
                        "product scenario {scenario}/{topology} step {} has an invalid marker",
                        step.step
                    );
                }
                if step.requires.is_empty() {
                    bail!(
                        "product scenario {scenario}/{topology} step {} has no prerequisite",
                        step.step
                    );
                }
                let requires_start = step.requires.iter().any(|required| required == "START");
                if requires_start && step.requires.len() != 1 {
                    bail!(
                        "product scenario {scenario}/{topology} step {} mixes START with step prerequisites",
                        step.step
                    );
                }
                let transition = self
                    .transitions
                    .iter()
                    .find(|transition| {
                        transition.flow == step.flow && transition.id == step.transition
                    })
                    .with_context(|| {
                        format!(
                            "product scenario {scenario}/{topology} step {} uses unknown transition {}/{}",
                            step.step, step.flow, step.transition
                        )
                    })?;
                if requires_start {
                    if transition.from != "START" {
                        bail!(
                            "product scenario {scenario}/{topology} root step {} must transition from START, found {}",
                            step.step,
                            transition.from
                        );
                    }
                } else {
                    let mut predecessor_states = BTreeSet::new();
                    let mut prerequisite_names = BTreeSet::new();
                    for required in &step.requires {
                        if !prerequisite_names.insert(required.as_str()) {
                            bail!(
                                "product scenario {scenario}/{topology} step {} repeats prerequisite {required}",
                                step.step
                            );
                        }
                        let Some((state, deadline_ms)) = completed.get(required.as_str()) else {
                            bail!(
                                "product scenario {scenario}/{topology} step {} requires missing or later step {required}",
                                step.step
                            );
                        };
                        if *deadline_ms > step.deadline_ms {
                            bail!(
                                "product scenario {scenario}/{topology} step {} deadline {} ms precedes prerequisite {required} deadline {} ms",
                                step.step,
                                step.deadline_ms,
                                deadline_ms
                            );
                        }
                        predecessor_states.insert(*state);
                    }
                    if !predecessor_states.contains(transition.from.as_str()) {
                        bail!(
                            "product scenario {scenario}/{topology} step {} transition starts at {}, outside prerequisite states {:?}",
                            step.step,
                            transition.from,
                            predecessor_states
                        );
                    }
                }
                if expected_sequence + 1 == steps.len() {
                    if transition.outcome != Outcome::Success {
                        bail!(
                            "product scenario {scenario}/{topology} terminal step {} is not a success outcome",
                            step.step
                        );
                    }
                } else if transition.outcome != Outcome::Continue {
                    bail!(
                        "product scenario {scenario}/{topology} intermediate step {} is not a continue outcome",
                        step.step
                    );
                }
                let model_refines_flow = transition.model == step.model
                    || self.model_bindings.iter().any(|binding| {
                        binding.model == step.model
                            && binding.flow == step.flow
                            && binding.role == "refinement"
                    });
                if !model_refines_flow {
                    bail!(
                        "product scenario {scenario}/{topology} step {} model {} does not refine flow {}",
                        step.step,
                        step.model,
                        step.flow
                    );
                }
                let model = self.models.get(&step.model).with_context(|| {
                    format!(
                        "product scenario {scenario}/{topology} step {} uses unknown model {}",
                        step.step, step.model
                    )
                })?;
                if !model.trace {
                    bail!(
                        "product scenario {scenario}/{topology} step {} uses model without runtime trace: {}",
                        step.step,
                        step.model
                    );
                }
                if transition.max_wait_ms > step.deadline_ms {
                    bail!(
                        "product scenario {scenario}/{topology} step {} absolute deadline {} ms is below transition {}/{} bound {} ms",
                        step.step,
                        step.deadline_ms,
                        step.flow,
                        step.transition,
                        transition.max_wait_ms
                    );
                }
                completed.insert(
                    step.step.as_str(),
                    (transition.to.as_str(), step.deadline_ms),
                );
            }
            let terminal = steps
                .last()
                .expect("validated product scenario group is nonempty");
            let mut reaches_terminal = BTreeSet::from([terminal.step.as_str()]);
            loop {
                let before = reaches_terminal.len();
                for step in steps.iter() {
                    if reaches_terminal.contains(step.step.as_str()) {
                        reaches_terminal.extend(step.requires.iter().map(String::as_str));
                    }
                }
                if reaches_terminal.len() == before {
                    break;
                }
            }
            let orphaned = steps
                .iter()
                .map(|step| step.step.as_str())
                .filter(|step| !reaches_terminal.contains(step))
                .collect::<Vec<_>>();
            if !orphaned.is_empty() {
                bail!(
                    "product scenario {scenario}/{topology} has branches not joined by terminal step {}: {}",
                    terminal.step,
                    orphaned.join(", ")
                );
            }
        }
        for (topology_name, topology) in &self.manifest.topologies {
            if topology.required_runtime_models.is_empty() || topology.required_artifacts.is_empty()
            {
                bail!("evidence topology must bind runtime models and binary artifacts");
            }
            if topology.runtime_scenario.trim().is_empty() || topology.hard_deadline_ms == 0 {
                bail!(
                    "evidence topology {topology_name} must bind a runtime scenario and hard deadline"
                );
            }
            let scenario_steps = scenario_groups
                .get(&(topology.runtime_scenario.as_str(), topology_name.as_str()))
                .with_context(|| {
                    format!(
                        "evidence topology {topology_name} lacks product scenario {}",
                        topology.runtime_scenario
                    )
                })?;
            let final_deadline = scenario_steps
                .last()
                .map(|step| step.deadline_ms)
                .unwrap_or(0);
            if final_deadline != topology.hard_deadline_ms {
                bail!(
                    "evidence topology {topology_name} deadline {} does not equal scenario terminal deadline {final_deadline}",
                    topology.hard_deadline_ms
                );
            }
            let scenario_models = scenario_steps
                .iter()
                .map(|step| step.model.as_str())
                .collect::<BTreeSet<_>>();
            let mut runtime_models = BTreeSet::new();
            for model in &topology.required_runtime_models {
                if !runtime_models.insert(model) {
                    bail!("evidence topology repeats runtime model {model}");
                }
                let Some(record) = self.models.get(model) else {
                    bail!("evidence topology refers to unknown model {model}");
                };
                if !record.trace {
                    bail!("evidence topology requires model without runtime trace: {model}");
                }
                if !scenario_models.contains(model.as_str()) {
                    bail!(
                        "evidence topology {topology_name} requires runtime model absent from scenario {}: {model}",
                        topology.runtime_scenario
                    );
                }
            }
            let mut artifacts = BTreeSet::new();
            for artifact in &topology.required_artifacts {
                validate_relative_contract_path(artifact, "topology artifact")?;
                if !artifacts.insert(artifact) {
                    bail!(
                        "evidence topology repeats binary artifact {}",
                        artifact.display()
                    );
                }
            }
        }
        for profile in self.manifest.profiles.values() {
            if profile.evidence_max_age_hours == 0 {
                bail!("evidence profile max age must be positive");
            }
            let mut evidence_paths = BTreeSet::new();
            for evidence in &profile.required_evidence {
                validate_relative_contract_path(evidence, "profile evidence")?;
                if !evidence_paths.insert(evidence) {
                    bail!("evidence profile repeats {}", evidence.display());
                }
            }
        }
        let intentional_terminal = self
            .models
            .values()
            .filter(|model| model.deadlock_policy == "intentional-terminal")
            .count();
        if intentional_terminal > self.manifest.max_intentional_terminal_models {
            bail!(
                "intentional-terminal model count regressed: {intentional_terminal} > {}",
                self.manifest.max_intentional_terminal_models
            );
        }
        for mapping in &self.manifest.source_mappings {
            if !root.join(&mapping.path).is_file() {
                bail!(
                    "formal source mapping does not exist: {}",
                    mapping.path.display()
                );
            }
            if mapping.models.is_empty() {
                bail!(
                    "formal source mapping has no models: {}",
                    mapping.path.display()
                );
            }
            for model in &mapping.models {
                if !self.models.contains_key(model) {
                    bail!(
                        "formal source mapping {} uses unknown model {model}",
                        mapping.path.display()
                    );
                }
            }
        }
        let mut risk_paths = BTreeSet::new();
        for surface in &self.manifest.risk_surfaces {
            if !root.join(&surface.path).is_file() {
                bail!(
                    "formal risk surface does not exist: {}",
                    surface.path.display()
                );
            }
            if !risk_paths.insert(&surface.path) {
                bail!(
                    "formal risk surface is repeated: {}",
                    surface.path.display()
                );
            }
            if !matches!(surface.severity.as_str(), "critical" | "high") {
                bail!(
                    "formal risk surface {} must be critical or high, not {}",
                    surface.path.display(),
                    surface.severity
                );
            }
            if surface.reason.trim().is_empty() {
                bail!(
                    "formal risk surface {} lacks an inclusion reason",
                    surface.path.display()
                );
            }
            if surface.flows.is_empty() {
                bail!(
                    "formal risk surface {} has no whole-flow coverage",
                    surface.path.display()
                );
            }
            let mut surface_flows = BTreeSet::new();
            for flow in &surface.flows {
                if !surface_flows.insert(flow) {
                    bail!(
                        "formal risk surface {} repeats flow {flow}",
                        surface.path.display()
                    );
                }
                if !flow_ids.contains(flow.as_str()) {
                    bail!(
                        "formal risk surface {} uses unknown whole flow {flow}",
                        surface.path.display()
                    );
                }
                let exact_transition = self.transitions.iter().any(|transition| {
                    transition.flow == *flow && transition.source == surface.path
                });
                let mapped_flow_model = self.manifest.source_mappings.iter().any(|mapping| {
                    mapping.path == surface.path
                        && mapping.models.iter().any(|model| {
                            self.transitions.iter().any(|transition| {
                                transition.flow == *flow && transition.model == *model
                            }) || self
                                .model_bindings
                                .iter()
                                .any(|binding| binding.model == *model && binding.flow == *flow)
                        })
                });
                if !exact_transition && !mapped_flow_model {
                    bail!(
                        "formal risk surface {} claims flow {flow} without an exact transition or model mapping",
                        surface.path.display()
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn impact(&self, paths: &[PathBuf]) -> ContractImpact {
        let mut impact = ContractImpact::default();
        for path in paths {
            let mut matched = false;
            for transition in &self.transitions {
                if transition.source == *path {
                    matched = true;
                    impact.models.insert(transition.model.clone());
                    if let Some(witness) = self.witnesses.iter().find(|witness| {
                        witness.model == transition.model
                            && witness.package == transition.witness_package
                            && witness.test == transition.witness_test
                    }) {
                        impact.witnesses.insert(witness.clone());
                    }
                }
            }
            for mapping in &self.manifest.source_mappings {
                if mapping.path == *path {
                    matched = true;
                    for model in &mapping.models {
                        impact.models.insert(model.clone());
                        impact.witnesses.extend(
                            self.witnesses
                                .iter()
                                .filter(|witness| witness.model == *model)
                                .cloned(),
                        );
                    }
                }
            }
            for surface in &self.manifest.risk_surfaces {
                if surface.path != *path {
                    continue;
                }
                matched = true;
                for transition in self
                    .transitions
                    .iter()
                    .filter(|transition| surface.flows.contains(&transition.flow))
                {
                    impact.models.insert(transition.model.clone());
                    if let Some(witness) = self.witnesses.iter().find(|witness| {
                        witness.model == transition.model
                            && witness.package == transition.witness_package
                            && witness.test == transition.witness_test
                    }) {
                        impact.witnesses.insert(witness.clone());
                    }
                }
            }
            if !matched && is_unregistered_high_risk_source(path) {
                impact.unmapped_high_risk.insert(path.clone());
            }
        }
        impact
    }

    pub(crate) fn generated_doc(&self) -> String {
        let mut output = String::from(
            "<!-- Generated by `cargo xtask formal-contracts generate`; do not edit. -->\n\
             # Formal Contract Registry\n\n",
        );
        let registry_hash = registry_hash(self);
        let apalache = self.models.values().filter(|model| model.apalache).count();
        let tlaps = self.models.values().filter(|model| model.tlaps).count();
        let runtime_trace = self.models.values().filter(|model| model.trace).count();
        let intentional_terminal = self
            .models
            .values()
            .filter(|model| model.deadlock_policy == "intentional-terminal")
            .count();
        let cyclic_sccs = self
            .flow_graphs()
            .values()
            .map(|graph| cyclic_scc_count(graph))
            .sum::<usize>();
        output.push_str(&format!(
            "- Schema: `{}`\n- Registry SHA-256: `{registry_hash}`\n\
            - Models: `{}`\n- Whole flows: `{}`\n- Transitions: `{}`\n\
             - Product runtime scenarios: `{}`\n\
             - Exact source witnesses: `{}`\n- Apalache pilots: `{apalache}`\n\
             - TLAPS theorem models: `{tlaps}`\n- Runtime-traced models: `{runtime_trace}`\n\
             - Intentional-terminal exceptions: `{intentional_terminal}` (ceiling `{}`)\n\
             - Cyclic strongly connected components: `{cyclic_sccs}`\n\
             - Supporting model bindings: `{}`\n\
             - Explicit critical/high risk surfaces: `{}`\n\
             - Additional source mappings: `{}`\n\n",
            self.manifest.schema,
            self.models.len(),
            self.flow_count(),
            self.transitions.len(),
            scenario_groups_count(&self.scenarios),
            self.witnesses.len(),
            self.manifest.max_intentional_terminal_models,
            self.model_bindings.len(),
            self.manifest.risk_surfaces.len(),
            self.manifest.source_mappings.len()
        ));
        output.push_str(
            "| Flow | Severity | Owners | Models | Requirements | Hazards | Sinks |\n\
             | --- | --- | --- | --- | ---: | ---: | --- |\n",
        );
        for (flow, graph) in self.flow_graphs() {
            let severity = graph
                .transitions
                .iter()
                .map(|transition| transition.severity)
                .min()
                .unwrap_or(Severity::Low);
            let owners = graph
                .transitions
                .iter()
                .map(|transition| transition.owner.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ");
            let models = graph
                .transitions
                .iter()
                .map(|transition| transition.model.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ");
            let sinks = graph
                .nodes
                .iter()
                .filter(|state| graph.outgoing.get(**state).is_none_or(BTreeSet::is_empty))
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(&format!(
                "| `{flow}` | `{}` | {owners} | {models} | {} | {} | {sinks} |\n",
                severity.as_str(),
                graph.transitions.len(),
                graph.transitions.len()
            ));
        }
        output
    }

    pub(crate) fn write_generated_doc(&self, root: &Path) -> Result<()> {
        let path = root.join(&self.manifest.generated_doc);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, self.generated_doc()).with_context(|| format!("write {}", path.display()))
    }

    pub(crate) fn check_generated_doc(&self, root: &Path) -> Result<()> {
        let path = root.join(&self.manifest.generated_doc);
        let actual =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let expected = self.generated_doc();
        if actual != expected {
            bail!(
                "{} is stale; run `cargo xtask formal-contracts generate`",
                self.manifest.generated_doc.display()
            );
        }
        Ok(())
    }

    fn flow_graphs(&self) -> BTreeMap<&str, FlowGraph<'_>> {
        let mut grouped: BTreeMap<&str, Vec<&Transition>> = BTreeMap::new();
        for transition in &self.transitions {
            grouped
                .entry(transition.flow.as_str())
                .or_default()
                .push(transition);
        }
        grouped
            .into_iter()
            .map(|(flow, transitions)| {
                let mut nodes = BTreeSet::new();
                let mut outgoing: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
                let mut incoming: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
                for transition in &transitions {
                    nodes.insert(transition.from.as_str());
                    nodes.insert(transition.to.as_str());
                    outgoing
                        .entry(transition.from.as_str())
                        .or_default()
                        .insert(transition.to.as_str());
                    incoming
                        .entry(transition.to.as_str())
                        .or_default()
                        .insert(transition.from.as_str());
                }
                (
                    flow,
                    FlowGraph {
                        transitions,
                        nodes,
                        outgoing,
                        incoming,
                    },
                )
            })
            .collect()
    }
}

fn validate_flow_graph(flow: &str, graph: &FlowGraph<'_>) -> Result<()> {
    if !graph.nodes.contains("START") {
        bail!("flow {flow} has no START state");
    }
    let sinks = graph
        .nodes
        .iter()
        .filter(|state| graph.outgoing.get(**state).is_none_or(BTreeSet::is_empty))
        .copied()
        .collect::<BTreeSet<_>>();
    if sinks.is_empty() {
        bail!("flow {flow} has no graph sink");
    }
    let reachable = closure("START", &graph.outgoing);
    let unreachable = graph
        .nodes
        .difference(&reachable)
        .copied()
        .collect::<Vec<_>>();
    if !unreachable.is_empty() {
        bail!(
            "flow {flow} has states unreachable from START: {}",
            unreachable.join(", ")
        );
    }
    let mut can_reach_sink = sinks.clone();
    let mut queue = sinks.iter().copied().collect::<VecDeque<_>>();
    while let Some(state) = queue.pop_front() {
        if let Some(predecessors) = graph.incoming.get(state) {
            for predecessor in predecessors {
                if can_reach_sink.insert(predecessor) {
                    queue.push_back(predecessor);
                }
            }
        }
    }
    let nonconvergent = reachable
        .difference(&can_reach_sink)
        .copied()
        .collect::<Vec<_>>();
    if !nonconvergent.is_empty() {
        bail!(
            "flow {flow} has states with no path to a terminal sink: {}",
            nonconvergent.join(", ")
        );
    }
    for component in strongly_connected_components(graph)
        .into_iter()
        .filter(|component| component_is_cyclic(component, graph))
    {
        let has_exit = component.iter().any(|state| {
            graph
                .outgoing
                .get(state)
                .is_some_and(|targets| targets.iter().any(|target| !component.contains(target)))
        });
        if !has_exit {
            bail!(
                "flow {flow} has a closed cyclic SCC with no terminal exit: {}",
                component.iter().copied().collect::<Vec<_>>().join(", ")
            );
        }
    }
    // An observable operation outcome may be terminal for one request while
    // the owning lifecycle continues through revoke/rebind. Use graph sinks
    // for convergence, but count all explicit non-continue operation outcomes
    // for the critical-flow denial/failure diversity gate.
    let terminal_outcomes = graph
        .transitions
        .iter()
        .map(|transition| transition.outcome)
        .filter(|outcome| *outcome != Outcome::Continue)
        .collect::<BTreeSet<_>>();
    let critical = graph
        .transitions
        .iter()
        .any(|transition| transition.severity == Severity::Critical);
    if critical && terminal_outcomes.len() < 2 && flow != "acpi-firmware-admission" {
        bail!("critical flow {flow} has fewer than two terminal sink outcome classes");
    }
    Ok(())
}

fn cyclic_scc_count(graph: &FlowGraph<'_>) -> usize {
    strongly_connected_components(graph)
        .iter()
        .filter(|component| component_is_cyclic(component, graph))
        .count()
}

fn component_is_cyclic(component: &BTreeSet<&str>, graph: &FlowGraph<'_>) -> bool {
    component.len() > 1
        || component.iter().any(|state| {
            graph
                .outgoing
                .get(state)
                .is_some_and(|targets| targets.contains(state))
        })
}

fn strongly_connected_components<'a>(graph: &FlowGraph<'a>) -> Vec<BTreeSet<&'a str>> {
    fn finish_order<'a>(
        state: &'a str,
        edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
        visited: &mut BTreeSet<&'a str>,
        order: &mut Vec<&'a str>,
    ) {
        if !visited.insert(state) {
            return;
        }
        if let Some(targets) = edges.get(state) {
            for target in targets {
                finish_order(target, edges, visited, order);
            }
        }
        order.push(state);
    }

    fn collect_component<'a>(
        state: &'a str,
        reverse_edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
        visited: &mut BTreeSet<&'a str>,
        component: &mut BTreeSet<&'a str>,
    ) {
        if !visited.insert(state) {
            return;
        }
        component.insert(state);
        if let Some(predecessors) = reverse_edges.get(state) {
            for predecessor in predecessors {
                collect_component(predecessor, reverse_edges, visited, component);
            }
        }
    }

    let mut visited = BTreeSet::new();
    let mut order = Vec::new();
    for state in &graph.nodes {
        finish_order(state, &graph.outgoing, &mut visited, &mut order);
    }
    visited.clear();
    let mut components = Vec::new();
    while let Some(state) = order.pop() {
        if visited.contains(state) {
            continue;
        }
        let mut component = BTreeSet::new();
        collect_component(state, &graph.incoming, &mut visited, &mut component);
        components.push(component);
    }
    components
}

fn closure<'a>(start: &'a str, edges: &BTreeMap<&'a str, BTreeSet<&'a str>>) -> BTreeSet<&'a str> {
    let mut visited = BTreeSet::from([start]);
    let mut queue = VecDeque::from([start]);
    while let Some(state) = queue.pop_front() {
        if let Some(successors) = edges.get(state) {
            for successor in successors {
                if visited.insert(successor) {
                    queue.push_back(successor);
                }
            }
        }
    }
    visited
}

fn is_unregistered_high_risk_source(path: &Path) -> bool {
    let path = path.to_string_lossy();
    if !path.ends_with(".rs") {
        return false;
    }
    let in_authority_boundary = path.starts_with("kernel/")
        || path.starts_with("services/")
        || path.starts_with("libs/rustos-user-abi/")
        || path.starts_with("libs/driver-domain-")
        || path.starts_with("tools/hostd/");
    if !in_authority_boundary {
        return false;
    }
    let file = path.rsplit('/').next().unwrap_or_default();
    matches!(
        file,
        "api.rs"
            | "main.rs"
            | "scheduler.rs"
            | "process_table.rs"
            | "process_state.rs"
            | "current.rs"
            | "irq.rs"
            | "handlers.rs"
            | "ipc.rs"
            | "memfd.rs"
    ) || file.ends_with("_ops.rs")
        || path.contains("/broker_ops/")
        || path.contains("/io/dvm_")
        || path.contains("/input/dvm_")
}

fn parse_models(path: &Path) -> Result<BTreeMap<String, ModelRecord>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut models = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 10 {
            bail!(
                "{}:{} has {} model fields",
                path.display(),
                index + 1,
                fields.len()
            );
        }
        let model = ModelRecord {
            name: fields[0].to_owned(),
            class: fields[1].to_owned(),
            deadlock_policy: fields[2].to_owned(),
            deadlock_reason: fields[3].to_owned(),
            pr_timeout_s: fields[4]
                .parse()
                .with_context(|| format!("{}:{} invalid PR timeout", path.display(), index + 1))?,
            nightly_timeout_s: fields[5].parse().with_context(|| {
                format!("{}:{} invalid nightly timeout", path.display(), index + 1)
            })?,
            nightly_mode: fields[6].to_owned(),
            apalache: parse_flag(fields[7])?,
            tlaps: parse_flag(fields[8])?,
            trace: parse_flag(fields[9])?,
        };
        if !matches!(model.class.as_str(), "safety" | "temporal") {
            bail!("invalid model class for {}: {}", model.name, model.class);
        }
        if !matches!(
            model.deadlock_policy.as_str(),
            "check" | "intentional-terminal"
        ) {
            bail!(
                "invalid deadlock policy for {}: {}",
                model.name,
                model.deadlock_policy
            );
        }
        if model.deadlock_policy == "intentional-terminal" && model.deadlock_reason.is_empty() {
            bail!("intentional-terminal model lacks rationale: {}", model.name);
        }
        if model.pr_timeout_s == 0 || model.nightly_timeout_s == 0 {
            bail!("model timeout must be positive: {}", model.name);
        }
        if !matches!(
            model.nightly_mode.as_str(),
            "exhaustive" | "exhaustive+simulate"
        ) {
            bail!(
                "invalid nightly mode for {}: {}",
                model.name,
                model.nightly_mode
            );
        }
        if models.insert(model.name.clone(), model).is_some() {
            bail!("duplicate model registry entry {}", fields[0]);
        }
    }
    Ok(models)
}

fn parse_model_bindings(path: &Path) -> Result<Vec<ModelBinding>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut bindings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 4 {
            bail!(
                "{}:{} has {} model binding fields",
                path.display(),
                index + 1,
                fields.len()
            );
        }
        bindings.push(ModelBinding {
            model: fields[0].to_owned(),
            role: fields[1].to_owned(),
            flow: fields[2].to_owned(),
            rationale: fields[3].to_owned(),
        });
    }
    Ok(bindings)
}

fn parse_transitions(path: &Path) -> Result<Vec<Transition>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut transitions = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 15 {
            bail!(
                "{}:{} has {} transition fields",
                path.display(),
                index + 1,
                fields.len()
            );
        }
        transitions.push(Transition {
            flow: fields[0].to_owned(),
            id: fields[1].to_owned(),
            requirement: fields[2].to_owned(),
            hazard: fields[3].to_owned(),
            severity: Severity::parse(fields[4])?,
            owner: fields[5].to_owned(),
            from: fields[6].to_owned(),
            event: fields[7].to_owned(),
            to: fields[8].to_owned(),
            outcome: Outcome::parse(fields[9])?,
            max_wait_ms: fields[10]
                .parse()
                .with_context(|| format!("{}:{} invalid max wait", path.display(), index + 1))?,
            model: fields[11].to_owned(),
            source: PathBuf::from(fields[12]),
            witness_package: fields[13].to_owned(),
            witness_test: fields[14].to_owned(),
        });
    }
    Ok(transitions)
}

fn parse_product_scenarios(path: &Path) -> Result<Vec<ProductScenarioStep>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut steps = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 11 {
            bail!(
                "{}:{} has {} product scenario fields",
                path.display(),
                index + 1,
                fields.len()
            );
        }
        steps.push(ProductScenarioStep {
            scenario: fields[0].to_owned(),
            topology: fields[1].to_owned(),
            sequence: fields[2].parse().with_context(|| {
                format!("{}:{} invalid scenario sequence", path.display(), index + 1)
            })?,
            step: fields[3].to_owned(),
            flow: fields[4].to_owned(),
            transition: fields[5].to_owned(),
            model: fields[6].to_owned(),
            log: fields[7].to_owned(),
            marker: fields[8].to_owned(),
            requires: fields[9]
                .split(',')
                .map(str::trim)
                .filter(|required| !required.is_empty())
                .map(str::to_owned)
                .collect(),
            deadline_ms: fields[10].parse().with_context(|| {
                format!("{}:{} invalid scenario deadline", path.display(), index + 1)
            })?,
        });
    }
    if steps.is_empty() {
        bail!("product scenario registry is empty");
    }
    Ok(steps)
}

fn scenario_groups_count(steps: &[ProductScenarioStep]) -> usize {
    steps
        .iter()
        .map(|step| (step.scenario.as_str(), step.topology.as_str()))
        .collect::<BTreeSet<_>>()
        .len()
}

fn parse_witnesses(path: &Path) -> Result<BTreeSet<WitnessRecord>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut active = false;
    let mut witnesses = BTreeSet::new();
    for line in text.lines() {
        if line.contains("done <<'EOF'") {
            active = true;
            continue;
        }
        if !active {
            continue;
        }
        if line == "EOF" {
            break;
        }
        let fields = line.split('|').collect::<Vec<_>>();
        if !(3..=4).contains(&fields.len()) {
            bail!("invalid source witness row: {line}");
        }
        let witness = WitnessRecord {
            model: fields[0].to_owned(),
            package: fields[1].to_owned(),
            test: fields[2].to_owned(),
            features: fields.get(3).copied().unwrap_or("").to_owned(),
        };
        if !witnesses.insert(witness) {
            bail!("duplicate source witness row: {line}");
        }
    }
    if witnesses.is_empty() {
        bail!("source witness registry is empty");
    }
    Ok(witnesses)
}

fn parse_flag(value: &str) -> Result<bool> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => bail!("invalid formal registry flag: {value}"),
    }
}

fn validate_relative_contract_path(path: &Path, kind: &str) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!(
            "{kind} must be a normalized relative path: {}",
            path.display()
        );
    }
    Ok(())
}

fn registry_hash(registry: &ContractRegistry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(registry.manifest.schema.to_le_bytes());
    hasher.update(
        registry
            .manifest
            .max_intentional_terminal_models
            .to_le_bytes(),
    );
    for path in [
        &registry.manifest.models,
        &registry.manifest.model_bindings,
        &registry.manifest.flows,
        &registry.manifest.scenarios,
        &registry.manifest.witnesses,
        &registry.manifest.generated_doc,
    ] {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
    }
    for model in registry.models.values() {
        for field in [
            model.name.as_str(),
            model.class.as_str(),
            model.deadlock_policy.as_str(),
            model.deadlock_reason.as_str(),
            model.nightly_mode.as_str(),
        ] {
            hasher.update(field.as_bytes());
            hasher.update([0]);
        }
        hasher.update(model.pr_timeout_s.to_le_bytes());
        hasher.update(model.nightly_timeout_s.to_le_bytes());
        hasher.update([model.apalache as u8, model.tlaps as u8, model.trace as u8]);
    }
    for binding in &registry.model_bindings {
        for field in [
            binding.model.as_str(),
            binding.role.as_str(),
            binding.flow.as_str(),
            binding.rationale.as_str(),
        ] {
            hasher.update(field.as_bytes());
            hasher.update([0]);
        }
    }
    for transition in &registry.transitions {
        for value in [
            transition.flow.as_str(),
            transition.id.as_str(),
            transition.requirement.as_str(),
            transition.hazard.as_str(),
            transition.owner.as_str(),
            transition.from.as_str(),
            transition.event.as_str(),
            transition.to.as_str(),
            transition.outcome.as_str(),
            transition.model.as_str(),
        ] {
            hasher.update(value.as_bytes());
            hasher.update([0]);
        }
        hasher.update(transition.max_wait_ms.to_le_bytes());
    }
    for step in &registry.scenarios {
        for field in [
            step.scenario.as_str(),
            step.topology.as_str(),
            step.step.as_str(),
            step.flow.as_str(),
            step.transition.as_str(),
            step.model.as_str(),
            step.log.as_str(),
            step.marker.as_str(),
        ] {
            hasher.update(field.as_bytes());
            hasher.update([0]);
        }
        for required in &step.requires {
            hasher.update(required.as_bytes());
            hasher.update([0]);
        }
        hasher.update(step.sequence.to_le_bytes());
        hasher.update(step.deadline_ms.to_le_bytes());
    }
    for witness in &registry.witnesses {
        hasher.update(witness.model.as_bytes());
        hasher.update([0]);
        hasher.update(witness.package.as_bytes());
        hasher.update([0]);
        hasher.update(witness.test.as_bytes());
        hasher.update([0]);
        hasher.update(witness.features.as_bytes());
    }
    for mapping in &registry.manifest.source_mappings {
        hasher.update(mapping.path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        for model in &mapping.models {
            hasher.update(model.as_bytes());
            hasher.update([0]);
        }
    }
    for surface in &registry.manifest.risk_surfaces {
        hasher.update(surface.path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        hasher.update(surface.severity.as_bytes());
        hasher.update([0]);
        for flow in &surface.flows {
            hasher.update(flow.as_bytes());
            hasher.update([0]);
        }
        hasher.update(surface.reason.as_bytes());
        hasher.update([0]);
    }
    for (profile, contract) in &registry.manifest.profiles {
        hasher.update(profile.as_bytes());
        hasher.update([0]);
        hasher.update(contract.evidence_max_age_hours.to_le_bytes());
        for path in &contract.required_evidence {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
        }
    }
    for (topology, contract) in &registry.manifest.topologies {
        hasher.update(topology.as_bytes());
        hasher.update([0]);
        hasher.update(contract.runtime_scenario.as_bytes());
        hasher.update([0]);
        hasher.update(contract.hard_deadline_ms.to_le_bytes());
        for model in &contract.required_runtime_models {
            hasher.update(model.as_bytes());
            hasher.update([0]);
        }
        for path in &contract.required_artifacts {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
        }
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        BTreeMap, BTreeSet, FlowGraph, cyclic_scc_count, is_unregistered_high_risk_source,
    };

    #[test]
    fn strongly_connected_cycle_with_terminal_exit_is_counted_once() {
        let nodes = BTreeSet::from(["START", "a", "b", "done"]);
        let outgoing = BTreeMap::from([
            ("START", BTreeSet::from(["a"])),
            ("a", BTreeSet::from(["b"])),
            ("b", BTreeSet::from(["a", "done"])),
        ]);
        let incoming = BTreeMap::from([
            ("a", BTreeSet::from(["START", "b"])),
            ("b", BTreeSet::from(["a"])),
            ("done", BTreeSet::from(["b"])),
        ]);
        let graph = FlowGraph {
            transitions: Vec::new(),
            nodes,
            outgoing,
            incoming,
        };
        assert_eq!(cyclic_scc_count(&graph), 1);
    }

    #[test]
    fn high_risk_fallback_targets_stateful_boundaries_not_every_rust_file() {
        assert!(is_unregistered_high_risk_source(Path::new(
            "kernel/ps/src/multitask/scheduler.rs"
        )));
        assert!(is_unregistered_high_risk_source(Path::new(
            "services/newpolicyd/src/main.rs"
        )));
        assert!(is_unregistered_high_risk_source(Path::new(
            "kernel/compat/src/user/syscall/linux/new_broker_ops.rs"
        )));
        assert!(!is_unregistered_high_risk_source(Path::new(
            "services/uiserver/src/color.rs"
        )));
        assert!(!is_unregistered_high_risk_source(Path::new(
            "kernel/ps/src/debug_format.rs"
        )));
    }
}
