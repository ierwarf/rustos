use anyhow::{Context, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;

#[derive(Debug, Default, PartialEq, Eq)]
struct DevPlan {
    scopes: BTreeSet<&'static str>,
    now: Vec<String>,
    stable_batch: Vec<String>,
    notes: Vec<&'static str>,
}

impl DevPlan {
    fn push_now(&mut self, command: impl Into<String>) {
        push_unique(&mut self.now, command.into());
    }

    fn push_stable(&mut self, command: impl Into<String>) {
        push_unique(&mut self.stable_batch, command.into());
    }

    fn push_note(&mut self, note: &'static str) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

pub(crate) fn print_plan(root: &Path) -> Result<()> {
    let paths = changed_paths(root)?;
    let plan = classify_changes(root, &paths);

    println!("changed={}", paths.len());
    if paths.is_empty() {
        println!("scope=clean");
        println!("note=no working-tree changes; no validation command was inferred");
        return Ok(());
    }

    println!(
        "scope={}",
        plan.scopes.iter().copied().collect::<Vec<_>>().join(",")
    );
    println!("now:");
    for command in &plan.now {
        println!("  {command}");
    }
    if !plan.stable_batch.is_empty() {
        println!("stable-batch:");
        for command in &plan.stable_batch {
            println!("  {command}");
        }
    }
    for note in &plan.notes {
        println!("note={note}");
    }
    Ok(())
}

fn changed_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    collect_git_paths(
        root,
        &[
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=ACDMRTUXB",
            "HEAD",
            "--",
        ],
        &mut paths,
    )?;
    collect_git_paths(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &mut paths,
    )?;
    Ok(paths.into_iter().collect())
}

fn collect_git_paths(root: &Path, args: &[&str], paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(raw)
            .context("dev-plan does not accept non-UTF-8 repository paths")?;
        paths.insert(PathBuf::from(path));
    }
    Ok(())
}

fn classify_changes(root: &Path, paths: &[PathBuf]) -> DevPlan {
    let mut plan = DevPlan::default();
    if paths.is_empty() {
        return plan;
    }

    plan.push_now("git diff --check");

    let mut formal_models = BTreeSet::new();
    let mut formal_infrastructure_changed = false;
    let mut dvm_paths = Vec::new();
    let mut needs_rust_check = false;
    let mut needs_kvm_dry_run = false;
    let mut only_docs = true;

    for path in paths {
        let Some(path_text) = path.to_str() else {
            plan.scopes.insert("unknown");
            needs_rust_check = true;
            only_docs = false;
            continue;
        };

        if path.extension().is_some_and(|extension| extension == "sh") {
            plan.push_now(format!("bash -n -- {}", shell_quote(path_text)));
        }

        if is_documentation(path) {
            plan.scopes.insert("docs");
            continue;
        }
        only_docs = false;

        if path.starts_with("formal") {
            plan.scopes.insert("formal");
            match formal_model_name(root, path) {
                Some(model) => {
                    formal_models.insert(model);
                }
                None => formal_infrastructure_changed = true,
            }
            continue;
        }

        if path.starts_with("driver-domains/linux") {
            plan.scopes.insert("dvm");
            dvm_paths.push(path.clone());
            needs_kvm_dry_run = true;
            continue;
        }

        plan.scopes.insert("rustos");
        needs_rust_check = true;
        if path == Path::new("tools/xtask/src/kvm.rs")
            || path_text.starts_with("libs/driver-domain-")
            || path.starts_with("services/uiserver")
        {
            needs_kvm_dry_run = true;
        }
    }

    if only_docs {
        plan.push_note("documentation-only changes do not require an OS or DVM rebuild");
        return plan;
    }

    if formal_infrastructure_changed {
        plan.push_now("bash formal/run-all-tlc.sh");
    } else {
        for model in formal_models {
            plan.push_now(format!("bash formal/run-tlc.sh {model}"));
        }
    }

    if needs_rust_check {
        plan.push_now("cargo xtask check");
    }

    if !dvm_paths.is_empty() {
        add_dvm_plan(&mut plan, &dvm_paths);
    }

    if needs_kvm_dry_run {
        plan.push_stable("cargo xtask kvm-smoke --dry-run");
    }

    if !plan.stable_batch.is_empty() {
        plan.push_note("run stable-batch once after the related change set settles; it is not an every-edit loop");
    }
    plan.push_note("dev-plan selects commands but never counts as validation evidence by itself");
    plan
}

fn add_dvm_plan(plan: &mut DevPlan, paths: &[PathBuf]) {
    let mut local_package = None;
    let local_source_only = paths.iter().all(|path| {
        let package = dvm_local_source_package(path);
        match (local_package, package) {
            (None, Some(found)) => {
                local_package = Some(found);
                true
            }
            (Some(expected), Some(found)) => expected == found,
            _ => false,
        }
    });
    let verify_scripts_only = paths.iter().all(|path| {
        path.to_str()
            .is_some_and(|path| path.starts_with("driver-domains/linux/scripts/verify-"))
            && path.extension().is_some_and(|extension| extension == "sh")
    });

    if verify_scripts_only {
        plan.push_stable("make -C driver-domains/linux verify");
        return;
    }

    if local_source_only {
        let target = match local_package.expect("local source package must be known") {
            "rustos-dvm-agent" => "dev-agent",
            "rustos-dvm-display" => "dev-display",
            "rustos-dvm-net" => "dev-net",
            _ => unreachable!("dvm_local_source_package returns only known packages"),
        };
        plan.push_now(format!("make -C driver-domains/linux {target}"));
        plan.push_note("dev-* compiles one cached DVM package only and deliberately blocks artifact verification until the final rebuild-*");
        let release_target = target.strip_prefix("dev-").expect("known dev target");
        plan.push_stable(format!(
            "make -C driver-domains/linux rebuild-{release_target}"
        ));
    } else {
        plan.push_stable("cargo xtask build-dvm");
    }
    plan.push_stable("cargo xtask verify-dvm");
}

fn dvm_local_source_package(path: &Path) -> Option<&'static str> {
    const PACKAGES: [&str; 3] = ["rustos-dvm-agent", "rustos-dvm-display", "rustos-dvm-net"];
    for package in PACKAGES {
        let prefix = Path::new("driver-domains/linux/package")
            .join(package)
            .join("src");
        if path.starts_with(prefix)
            && path
                .extension()
                .is_some_and(|extension| extension == "c" || extension == "h")
        {
            return Some(package);
        }
    }
    None
}

fn formal_model_name(root: &Path, path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?;
    if extension != "tla" && extension != "cfg" {
        return None;
    }
    let relative = path.strip_prefix("formal").ok()?.with_extension("");
    let model = relative.to_str()?;
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_'))
    {
        return None;
    }
    let spec = root.join("formal").join(&relative).with_extension("tla");
    let config = root.join("formal").join(&relative).with_extension("cfg");
    (spec.is_file() && config.is_file()).then(|| model.to_owned())
}

fn is_documentation(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "md")
        || matches!(
            path.to_str(),
            Some("AGENTS.md" | "CODE_OF_CONDUCT.md" | "CONTRIBUTING.md" | "SECURITY.md")
        )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn push_unique(commands: &mut Vec<String>, command: String) {
    if !commands.contains(&command) {
        commands.push(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn docs_only_skips_builds() {
        let root = tempfile::tempdir().unwrap();
        let plan = classify_changes(root.path(), &[PathBuf::from("docs/ai/commands.md")]);
        assert_eq!(plan.now, vec!["git diff --check"]);
        assert!(plan.stable_batch.is_empty());
        assert!(plan.scopes.contains("docs"));
    }

    #[test]
    fn formal_model_change_is_focused() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("formal/example");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("Example.tla"), "---- MODULE Example ----").unwrap();
        fs::write(directory.join("Example.cfg"), "SPECIFICATION Spec").unwrap();

        let plan = classify_changes(root.path(), &[PathBuf::from("formal/example/Example.tla")]);
        assert!(
            plan.now
                .contains(&"bash formal/run-tlc.sh example/Example".to_owned())
        );
        assert!(!plan.now.contains(&"bash formal/run-all-tlc.sh".to_owned()));
    }

    #[test]
    fn formal_runner_change_runs_full_suite() {
        let root = tempfile::tempdir().unwrap();
        let plan = classify_changes(root.path(), &[PathBuf::from("formal/run-tlc.sh")]);
        assert!(plan.now.contains(&"bash formal/run-all-tlc.sh".to_owned()));
        assert!(
            plan.now
                .contains(&"bash -n -- 'formal/run-tlc.sh'".to_owned())
        );
    }

    #[test]
    fn one_dvm_source_package_uses_fast_compile_then_targeted_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let plan = classify_changes(
            root.path(),
            &[PathBuf::from(
                "driver-domains/linux/package/rustos-dvm-display/src/relay.c",
            )],
        );
        assert!(
            plan.now
                .contains(&"make -C driver-domains/linux dev-display".to_owned())
        );
        assert_eq!(
            plan.stable_batch,
            vec![
                "make -C driver-domains/linux rebuild-display",
                "cargo xtask verify-dvm",
                "cargo xtask kvm-smoke --dry-run",
            ]
        );
    }

    #[test]
    fn global_dvm_input_batches_one_full_build() {
        let root = tempfile::tempdir().unwrap();
        let plan = classify_changes(
            root.path(),
            &[
                PathBuf::from("driver-domains/linux/package/rustos-dvm-display/src/relay.c"),
                PathBuf::from("driver-domains/linux/configs/rustos_linux_dvm_x86_64_defconfig"),
            ],
        );
        assert_eq!(
            plan.stable_batch,
            vec![
                "cargo xtask build-dvm",
                "cargo xtask verify-dvm",
                "cargo xtask kvm-smoke --dry-run",
            ]
        );
    }

    #[test]
    fn rust_change_gets_fast_workspace_check() {
        let root = tempfile::tempdir().unwrap();
        let plan = classify_changes(root.path(), &[PathBuf::from("libs/example/src/lib.rs")]);
        assert!(plan.now.contains(&"cargo xtask check".to_owned()));
        assert!(plan.stable_batch.is_empty());
    }
}
