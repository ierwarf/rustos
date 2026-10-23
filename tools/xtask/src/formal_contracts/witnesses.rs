//! Execute the impacted native witnesses once per exact Cargo selection.
//!
//! Model-to-test associations remain in the registry. Deduplicating execution
//! does not deduplicate evidence: every requested test must print its own pass
//! record and the total executed count must match, including across bin targets.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, ensure};

use super::registry::WitnessRecord;
use crate::Result;

type Groups = BTreeMap<(String, String), BTreeSet<String>>;

fn groups(witnesses: &BTreeSet<WitnessRecord>) -> Groups {
    let mut result = Groups::new();
    for witness in witnesses {
        result
            .entry((witness.package.clone(), witness.features.clone()))
            .or_default()
            .insert(witness.test.clone());
    }
    result
}

fn command(root: &Path, package: &str, features: &str, tests: &BTreeSet<String>) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root).args(["test", "-p", package]);
    if !features.is_empty() {
        cmd.args(["--features", features]);
    }
    // Keep packages and feature selections separate: batching package flags
    // could change Cargo feature unification. Only libtest filters are batched.
    cmd.args(["--", "--exact"]).args(tests);
    if package == "kernel-ps" {
        // These host fixtures reset shared synthetic architecture state. They
        // are single-owner, just as in run-source-conformance.sh.
        cmd.env("RUST_TEST_THREADS", "1");
    }
    cmd
}

fn verify(output: &str, tests: &BTreeSet<String>) -> Result<()> {
    for test in tests {
        let normal = format!("test {test} ... ok");
        let panics = format!("test {test} - should panic ... ok");
        ensure!(
            output.lines().any(|line| line == normal || line == panics),
            "native witness did not execute and pass: {test}"
        );
    }
    let mut executed = 0usize;
    for line in output.lines() {
        if let Some(summary) = line.strip_prefix("test result: ok. ") {
            let (passed, rest) = summary
                .split_once(" passed; ")
                .context("malformed native test summary")?;
            ensure!(
                rest.starts_with("0 failed; "),
                "native test summary reports failure"
            );
            executed = executed
                .checked_add(passed.parse::<usize>()?)
                .context("native test count overflow")?;
        }
    }
    ensure!(
        executed == tests.len(),
        "native witnesses executed {executed} tests, expected {}",
        tests.len()
    );
    Ok(())
}

pub(super) fn run(root: &Path, witnesses: &BTreeSet<WitnessRecord>) -> Result<()> {
    let selections = groups(witnesses);
    let mut count = 0usize;
    for ((package, features), tests) in &selections {
        let started = std::time::Instant::now();
        let output = command(root, package, features, tests)
            .output()
            .with_context(|| format!("run native witnesses for {package}"))?;
        let text = String::from_utf8_lossy(&output.stdout);
        let checked = if output.status.success() {
            verify(&text, tests)
        } else {
            anyhow::bail!(
                "native witness Cargo selection failed: {package} [{features}]\n{}\n{}",
                text,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        checked.with_context(|| format!("native witnesses for {package} [{features}]\n{text}"))?;
        count += tests.len();
        println!(
            "native witnesses passed package={package} features={features:?} tests={} elapsed_seconds={:.3}",
            tests.len(), started.elapsed().as_secs_f64()
        );
    }
    println!(
        "native witness batch passed selections={} tests={count} associations={}",
        selections.len(),
        witnesses.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn duplicate_model_associations_execute_once_but_feature_selections_stay_separate() {
        let mut rows = BTreeSet::new();
        for (model, features) in [("first", ""), ("second", ""), ("third", "host-test")] {
            rows.insert(WitnessRecord {
                model: model.into(),
                package: "owner".into(),
                test: "tests::exact".into(),
                features: features.into(),
            });
        }
        let selected = groups(&rows);
        assert_eq!(selected.len(), 2);
        assert_eq!(
            selected[&("owner".into(), String::new())],
            names(&["tests::exact"])
        );
    }

    #[test]
    fn exact_pass_lines_and_total_are_both_required() {
        let selected = names(&["tests::first", "tests::second"]);
        let good = "test tests::first ... ok\ntest tests::second - should panic ... ok\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out\n";
        verify(good, &selected).unwrap();
        for bad in [
            good.replace("tests::first ...", "tests::first_extra ..."),
            good.replace("2 passed", "0 passed"),
            good.replace("2 passed", "3 passed"),
            good.replace(
                "tests::second - should panic ... ok",
                "tests::second ... ignored",
            ),
            good.replace("test result: ok.", "test result: FAILED."),
        ] {
            assert!(verify(&bad, &selected).is_err(), "{bad}");
        }
        assert!(verify("test result: ok. 0 passed; 0 failed; 0 ignored;", &selected).is_err());
    }

    #[test]
    fn totals_across_targets_cannot_hide_duplicate_execution() {
        let text = "test tests::first ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;\ntest result: ok. 1 passed; 0 failed; 0 ignored;";
        assert!(verify(text, &names(&["tests::first"])).is_err());
    }

    #[test]
    fn command_preserves_features_exact_filters_and_fixture_serialization() {
        let cmd = command(
            Path::new("."),
            "kernel-ps",
            "fixture",
            &names(&["tests::a", "tests::b"]),
        );
        let args: Vec<_> = cmd.get_args().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "test",
                "-p",
                "kernel-ps",
                "--features",
                "fixture",
                "--",
                "--exact",
                "tests::a",
                "tests::b"
            ]
        );
        assert!(
            cmd.get_envs().any(|(key, value)| key == "RUST_TEST_THREADS"
                && value == Some(std::ffi::OsStr::new("1")))
        );
        assert_eq!(
            command(Path::new("."), "other", "", &names(&["tests::a"]))
                .get_envs()
                .count(),
            0
        );
    }
}
