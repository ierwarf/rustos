// SPDX-License-Identifier: MIT

//! Startup-readiness evidence: the single parser for the typed records that
//! services print, so the harness decides "ready" from the owner's record and
//! not from free-text greps.
//!
//! Records consumed:
//! - `uiserver: readiness v1 state=<starting|ready|degraded|failed> gen=<n>
//!   [cause=<dependency> errno=<n>] assets=.. display=.. input=.. wayland=..
//!   runtime=.. gpu=..` (owner: `services/uiserver/src/startup.rs`).
//! - `initd: fatal service endpoint not ready exec=<path> pid=<n> errno=<n>
//!   [cause=<label> waited_ms=<n> slices=<n>]` (owner:
//!   `services/initd/src/endpoint_admission.rs`; the optional fields appear on
//!   initd builds with the typed admission wait).
//!
//! Contract: `docs/ai/ui-startup-readiness.md`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UiReadinessState {
    Starting,
    Ready,
    Degraded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UiReadiness {
    pub(super) state: UiReadinessState,
    pub(super) generation: u64,
    /// `(dependency, errno)` of the first required dependency that failed.
    pub(super) cause: Option<(String, i32)>,
    /// `(dependency, state-label)` in record order, e.g. `("input", "backoff#2")`.
    pub(super) dependencies: Vec<(String, String)>,
}

const UI_READINESS_PREFIX: &str = "uiserver: readiness v1 ";
const UI_DEPENDENCIES: [&str; 6] = ["assets", "display", "input", "wayland", "runtime", "gpu"];

fn parse_ui_readiness_record(line: &str) -> Option<UiReadiness> {
    let (_, fields) = line.split_once(UI_READINESS_PREFIX)?;
    let mut state = None;
    let mut generation = None;
    let mut cause_dep = None;
    let mut cause_errno = None;
    let mut dependencies = Vec::new();
    for field in fields.split_whitespace() {
        // The console wrapper terminates the payload with a literal `\n`.
        let field = field.trim_end_matches("\\n");
        let (key, value) = field.split_once('=')?;
        match key {
            "state" => {
                state = Some(match value {
                    "starting" => UiReadinessState::Starting,
                    "ready" => UiReadinessState::Ready,
                    "degraded" => UiReadinessState::Degraded,
                    "failed" => UiReadinessState::Failed,
                    _ => return None,
                })
            }
            "gen" => generation = Some(value.parse::<u64>().ok()?),
            "cause" => cause_dep = Some(value.to_owned()),
            "errno" => cause_errno = Some(value.parse::<i32>().ok()?),
            dep if UI_DEPENDENCIES.contains(&dep) => {
                dependencies.push((dep.to_owned(), value.to_owned()))
            }
            _ => return None,
        }
    }
    let state = state?;
    let cause = match (state, cause_dep, cause_errno) {
        (UiReadinessState::Failed, Some(dep), Some(errno)) => Some((dep, errno)),
        // A failed record must name its cause; any other state must not.
        (UiReadinessState::Failed, _, _) => return None,
        (_, None, None) => None,
        _ => return None,
    };
    (dependencies.len() == UI_DEPENDENCIES.len()).then_some(UiReadiness {
        state,
        generation: generation?,
        cause,
        dependencies,
    })
}

/// The newest record: the highest generation (the machine increases it on every
/// change, and the line is printed to both the debug and diag consoles, so equal
/// generations are the same state).
pub(super) fn latest_ui_readiness(log: &str) -> Option<UiReadiness> {
    log.lines()
        .filter_map(parse_ui_readiness_record)
        .max_by_key(|record| record.generation)
}

/// A precise reason when uiserver reported a terminal failure.
pub(super) fn ui_readiness_failure(log: &str) -> Option<String> {
    let record = latest_ui_readiness(log)?;
    let (dependency, errno) = record.cause.as_ref()?;
    let states = record
        .dependencies
        .iter()
        .map(|(name, state)| format!("{name}={state}"))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "uiserver readiness failed: dependency={dependency} errno={errno} gen={} ({states})",
        record.generation
    ))
}

/// A precise reason when initd stopped on a service endpoint that never came up.
pub(super) fn initd_endpoint_failure(log: &str) -> Option<String> {
    const MARKER: &str = "initd: fatal service endpoint not ready ";
    let (_, fields) = log.lines().find_map(|line| line.split_once(MARKER))?;
    let field = |name: &str| {
        fields.split_whitespace().find_map(|field| {
            let field = field.trim_end_matches("\\n");
            field
                .split_once('=')
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| value.to_owned())
        })
    };
    let exec = field("exec")?;
    let mut detail = format!("initd endpoint admission failed: exec={exec}");
    for name in ["pid", "errno", "cause", "waited_ms", "slices"] {
        if let Some(value) = field(name) {
            detail.push_str(&format!(" {name}={value}"));
        }
    }
    Some(detail)
}

/// The first startup failure in `log` as a human-readable cause, if any.
pub(super) fn startup_failure_detail(log: &str) -> Option<String> {
    initd_endpoint_failure(log).or_else(|| ui_readiness_failure(log))
}

/// True when a UI proof run may rely on the UI having completed startup.
pub(super) fn ui_readiness_settled(log: &str) -> bool {
    latest_ui_readiness(log).is_some_and(|record| {
        matches!(
            record.state,
            UiReadinessState::Ready | UiReadinessState::Degraded
        )
    })
}

/// One observable step of the boot-to-desktop path, in causal order.
///
/// `marker` is a substring of a line the owning component prints. `optional`
/// stages exist only on builds that print them; they count as reached when
/// present but are never reported as the first missing stage, so an older log
/// is not blamed for a line it could not contain.
struct BootStage {
    name: &'static str,
    marker: &'static str,
    optional: bool,
}

const fn stage(name: &'static str, marker: &'static str) -> BootStage {
    BootStage {
        name,
        marker,
        optional: false,
    }
}

const fn optional_stage(name: &'static str, marker: &'static str) -> BootStage {
    BootStage {
        name,
        marker,
        optional: true,
    }
}

const BOOT_STAGES: &[BootStage] = &[
    stage("rootd-core-ready", "rootd: core services ready, spawning initd"),
    stage("initd-supervising", "initd: supervisor loop enter"),
    stage("netd-spawned", "initd: loader spawn returned exec=services/netd/netd.elf"),
    stage(
        "devmgrd-spawned",
        "initd: loader spawn returned exec=services/devmgrd/devmgrd.elf",
    ),
    stage(
        "inputd-spawned",
        "initd: loader spawn returned exec=services/inputd/inputd.elf",
    ),
    stage("devmgrd-running", "devmgrd: service start"),
    optional_stage("devmgrd-workers-started", "devmgrd: ioctl workers started"),
    stage("devmgrd-registered", "devmgrd: device policy endpoint registered"),
    stage("inputd-registered", "inputd: input policy endpoint registered"),
    optional_stage("inputd-ingress-running", "inputd: DVM ingress worker running"),
    stage("input-policy-consumer-live", "name=dvm-input-policy-ready"),
    stage(
        "devmgrd-admitted",
        "initd: service endpoint ready exec=services/devmgrd/devmgrd.elf",
    ),
    stage("runtimed-told-ui-ready", "ui ready received"),
    stage("uiserver-settled", "uiserver: readiness v1 state=ready"),
    stage(
        "wayclick-spawned",
        "loaderd: spawn done exec=/apps/wayclick/wayclick.elf",
    ),
    stage("wayclick-running", "wayclick: main enter"),
    stage("wayclick-first-frame", "wayclick: first frame presented"),
];

/// Where a boot stopped: the last stage that was reached and the first required
/// stage after the start that was not. A stage after a missing one can still be
/// present (stages overlap), so both are reported instead of one cut-off point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BootProgress {
    pub(super) last_reached: Option<&'static str>,
    pub(super) first_missing: Option<&'static str>,
}

pub(super) fn boot_progress(log: &str) -> BootProgress {
    let mut last_reached = None;
    let mut first_missing = None;
    for stage in BOOT_STAGES {
        if log.contains(stage.marker) {
            last_reached = Some(stage.name);
        } else if !stage.optional && first_missing.is_none() {
            first_missing = Some(stage.name);
        }
    }
    BootProgress {
        last_reached,
        first_missing,
    }
}

/// One-line attribution for a run that did not reach readiness. It names a
/// component's missing step; it does not claim a cause.
pub(super) fn boot_stage_summary(log: &str) -> String {
    let progress = boot_progress(log);
    match progress.first_missing {
        None => "boot-stage: every observed stage reached".to_owned(),
        Some(missing) => format!(
            "boot-stage: first-missing={missing} last-reached={}",
            progress.last_reached.unwrap_or("none")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real console line of lane0 run 45 (`1794210185000-916407.log`).
    const INITD_RUN_45: &str = "user-debug payload=initd: fatal service endpoint not ready exec=services/devmgrd/devmgrd.elf pid=25 errno=110\\n";
    // Same wrapper, emitted by the typed admission wait.
    const INITD_TYPED: &str = "user-debug payload=initd: fatal service endpoint not ready exec=services/devmgrd/devmgrd.elf pid=25 errno=110 cause=budget-exhausted waited_ms=30000 slices=6\\n";
    // Format asserted by `startup::tests` in uiserver.
    const READY: &str = "user-debug payload=uiserver: readiness v1 state=ready gen=14 assets=ready#1 display=ready#1 input=ready#1 wayland=ready#1 runtime=ready#1 gpu=ready#1\\n";
    const FAILED: &str = "user-debug payload=uiserver: readiness v1 state=failed gen=9 cause=input errno=110 assets=ready#1 display=ready#1 input=failed#7/errno110 wayland=ready#1 runtime=backoff#2 gpu=attempting#1\\n";

    #[test]
    fn initd_failure_from_the_real_run_45_line_names_service_and_errno() {
        let detail = initd_endpoint_failure(INITD_RUN_45).expect("failure");
        assert_eq!(
            detail,
            "initd endpoint admission failed: exec=services/devmgrd/devmgrd.elf pid=25 errno=110"
        );
    }

    #[test]
    fn typed_initd_failure_reports_cause_and_time_waited() {
        let detail = initd_endpoint_failure(INITD_TYPED).expect("failure");
        assert!(detail.contains("cause=budget-exhausted"), "{detail}");
        assert!(detail.contains("waited_ms=30000"), "{detail}");
        assert!(detail.contains("slices=6"), "{detail}");
    }

    #[test]
    fn a_failed_record_names_the_failed_dependency_and_errno() {
        let detail = ui_readiness_failure(FAILED).expect("failure");
        assert!(detail.contains("dependency=input errno=110"), "{detail}");
        assert!(detail.contains("runtime=backoff#2"), "{detail}");
        assert_eq!(startup_failure_detail(FAILED), Some(detail));
    }

    #[test]
    fn the_newest_generation_wins_whatever_the_line_order() {
        let log = format!("{READY}\n{FAILED}\n");
        let latest = latest_ui_readiness(&log).expect("record");
        assert_eq!(latest.generation, 14);
        assert_eq!(latest.state, UiReadinessState::Ready);
        assert!(ui_readiness_failure(&log).is_none());
        assert!(ui_readiness_settled(&log));
    }

    #[test]
    fn a_starting_or_missing_record_is_not_settled() {
        let starting = "uiserver: readiness v1 state=starting gen=2 assets=attempting#1 display=pending input=pending wayland=pending runtime=pending gpu=pending";
        assert!(!ui_readiness_settled(starting));
        assert!(!ui_readiness_settled("uiserver: ui assets loaded"));
        assert!(startup_failure_detail(starting).is_none());
    }

    #[test]
    fn degraded_counts_as_settled_and_is_not_a_failure() {
        let degraded = READY
            .replace("state=ready", "state=degraded")
            .replace("assets=ready#1", "assets=failed#3/errno2");
        assert!(ui_readiness_settled(&degraded));
        assert!(ui_readiness_failure(&degraded).is_none());
    }

    #[test]
    fn malformed_records_are_ignored_not_guessed() {
        for bad in [
            "uiserver: readiness v1 state=failed gen=1 assets=ready display=ready input=ready wayland=ready runtime=ready gpu=ready",
            "uiserver: readiness v1 state=ready gen=x assets=a display=b input=c wayland=d runtime=e gpu=f",
            "uiserver: readiness v1 state=ready gen=1 cause=input errno=1 assets=a display=b input=c wayland=d runtime=e gpu=f",
            "uiserver: readiness v1 state=ready gen=1 assets=a",
            "uiserver: readiness v2 state=ready gen=1",
        ] {
            assert!(parse_ui_readiness_record(bad).is_none(), "{bad}");
        }
    }

    // Real lines of the round-2 failures (4 vCPU, 2026-11-09).
    // lane2 run 28: rootd never got an initd.
    const ROOTD_STUCK: &str = "user-debug payload=rootd: job memory charge ok\\n\nuser-debug payload=rootd: core services ready, spawning initd via loaderd\\n\n";
    // lane3 run 22: inputd registered, its ingestion worker never armed.
    const INPUTD_NO_CONSUMER: &str = "\
user-debug payload=rootd: core services ready, spawning initd via loaderd\\n
user-debug payload=initd: supervisor loop enter\\n
user-debug payload=initd: loader spawn returned exec=services/netd/netd.elf pid=24\\n
user-debug payload=initd: loader spawn returned exec=services/devmgrd/devmgrd.elf pid=25\\n
user-debug payload=initd: loader spawn returned exec=services/inputd/inputd.elf pid=26\\n
user-debug payload=inputd: input policy endpoint registered\\n
user-debug payload=devmgrd: service start\\n
";

    #[test]
    fn a_boot_that_never_started_initd_is_attributed_to_rootd_not_inputd() {
        let progress = boot_progress(ROOTD_STUCK);
        assert_eq!(progress.last_reached, Some("rootd-core-ready"));
        assert_eq!(progress.first_missing, Some("initd-supervising"));
        assert_eq!(
            boot_stage_summary(ROOTD_STUCK),
            "boot-stage: first-missing=initd-supervising last-reached=rootd-core-ready"
        );
    }

    #[test]
    fn a_registered_inputd_without_a_live_consumer_and_a_silent_devmgrd_are_separated() {
        let progress = boot_progress(INPUTD_NO_CONSUMER);
        assert_eq!(progress.first_missing, Some("devmgrd-registered"));
        assert_eq!(progress.last_reached, Some("inputd-registered"));
        let with_consumer = format!(
            "{INPUTD_NO_CONSUMER}devmgrd: device policy endpoint registered\ninputd: DVM ingress worker running\nname=dvm-input-policy-ready\n"
        );
        assert_eq!(
            boot_progress(&with_consumer).first_missing,
            Some("devmgrd-admitted")
        );
    }

    #[test]
    fn optional_stages_are_never_blamed_on_a_log_that_could_not_print_them() {
        let progress = boot_progress(INPUTD_NO_CONSUMER);
        assert_ne!(progress.first_missing, Some("inputd-ingress-running"));
        assert_ne!(progress.first_missing, Some("devmgrd-workers-started"));
    }

    #[test]
    fn a_spawned_app_that_never_reached_main_is_named() {
        let log = "uiserver: readiness v1 state=ready gen=13 assets=ready display=ready input=ready wayland=ready runtime=ready gpu=ready\nloaderd: spawn done exec=/apps/wayclick/wayclick.elf pid=83\n";
        let progress = boot_progress(log);
        assert_eq!(progress.last_reached, Some("wayclick-spawned"));
        assert!(boot_progress(&format!("{log}wayclick: main enter\n")).last_reached
            == Some("wayclick-running"));
    }
}
