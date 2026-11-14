//! Drift checks between hand-assigned identifiers and the places that repeat
//! them. The Rust constants are the single source; everything else is checked
//! against them by `cargo xtask check`:
//!
//! * native syscall numbers are unique, in range and never reuse a retired
//!   number (the same invariant as `formal/check-native-syscall-numbers.py`
//!   and the `rustos_syscall_numbers_are_unique` test, kept in step with both);
//! * ABI docs only quote registered `0x5255_xxxx` numbers, and a
//!   `SYS_RUSTOS_NAME (0x...)` mention carries NAME's current number;
//! * every kernel milestone marker the KVM harness waits for still appears as a
//!   literal in the sources that print it, so a rename fails here instead of
//!   becoming a probe timeout.

use anyhow::{Context, bail};
use fs_err as fs;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::Result;

const NATIVE_PREFIX: u64 = 0x5255_0000;

/// Doc files whose syscall numbers are checked. Dated history logs are
/// deliberately not scanned.
const DOC_GLOBS: &[&str] = &["docs/ai/abi", "docs/architecture/job-model.md"];

/// Marker constants whose value is not text printed by kernel or services.
/// Each entry is (const name, reason).
const MARKER_ALLOWLIST: &[(&str, &str)] = &[
    (
        "DVM_DEV_OUTPUT_MARKER",
        "a host build-output path stamp, not a log line",
    ),
];

pub(crate) fn check_abi_drift(root: &Path) -> Result<()> {
    let mut errors = Vec::new();
    let registry = load_syscalls(root, &mut errors)?;
    check_docs(root, &registry, &mut errors)?;
    check_markers(root, &mut errors)?;
    if errors.is_empty() {
        return Ok(());
    }
    bail!("ABI identifier drift check failed:\n  {}", errors.join("\n  "));
}

struct SyscallRegistry {
    by_name: BTreeMap<String, u64>,
    retired: Vec<u64>,
}

fn load_syscalls(root: &Path, errors: &mut Vec<String>) -> Result<SyscallRegistry> {
    let abi = root.join("libs/rustos-user-abi/src");
    let mut files = vec![abi.join("syscall.rs"), abi.join("pager.rs")];
    for entry in fs::read_dir(abi.join("syscall"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    let retired: Vec<u64> = rustos_user_abi::retired::RETIRED_NATIVE_SYSCALLS.to_vec();
    let mut by_name = BTreeMap::new();
    let mut by_number: BTreeMap<u64, String> = BTreeMap::new();
    for file in files {
        let text = fs::read_to_string(&file)?;
        for (name, number) in parse_syscall_declarations(&text, errors, &file) {
            if number & 0xFFFF_0000 != NATIVE_PREFIX {
                errors.push(format!("{}: {name}={number:#x} is outside 0x5255_XXXX", file.display()));
            }
            if retired.contains(&number) {
                errors.push(format!("{name}={number:#x} reuses a retired syscall number"));
            }
            if let Some(previous) = by_number.insert(number, name.clone()) {
                errors.push(format!("syscall {number:#x} is claimed by both {previous} and {name}"));
            }
            if by_name.insert(name.clone(), number).is_some() {
                errors.push(format!("duplicate syscall declaration {name}"));
            }
        }
    }
    if by_name.len() < 50 {
        errors.push(format!("syscall scan found only {} declarations", by_name.len()));
    }
    Ok(SyscallRegistry { by_name, retired })
}

/// Declarations are `pub const SYS_RUSTOS_X: u64 = 0x5255_xxxx;`, possibly with
/// the value on the next line. A non-literal value is an error: the number must
/// be written once, as a literal, where the name is declared.
fn parse_syscall_declarations(
    text: &str,
    errors: &mut Vec<String>,
    file: &Path,
) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("pub const SYS_RUSTOS_") {
        rest = &rest[at + "pub const ".len()..];
        let Some(colon) = rest.find(':') else { break };
        let name = rest[..colon].trim().to_string();
        let Some(end) = rest.find(';') else { break };
        let expression = rest[colon..end].split('=').nth(1).unwrap_or("").trim();
        match expression
            .strip_prefix("0x")
            .and_then(|hex| u64::from_str_radix(&hex.replace('_', ""), 16).ok())
        {
            Some(number) => out.push((name, number)),
            None => errors.push(format!(
                "{}: {name} is not one exact hex literal",
                file.display()
            )),
        }
    }
    out
}

fn check_docs(root: &Path, registry: &SyscallRegistry, errors: &mut Vec<String>) -> Result<()> {
    let mut files: Vec<PathBuf> = Vec::new();
    for glob in DOC_GLOBS {
        let path = root.join(glob);
        if path.is_dir() {
            for entry in fs::read_dir(&path)? {
                let p = entry?.path();
                if p.extension().is_some_and(|ext| ext == "md") {
                    files.push(p);
                }
            }
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    for file in files {
        let text = fs::read_to_string(&file)?;
        for (index, line) in text.lines().enumerate() {
            let at = format!("{}:{}", file.strip_prefix(root).unwrap_or(&file).display(), index + 1);
            let lines: Vec<&str> = text.lines().collect();
            let near = lines[index.saturating_sub(1)..(index + 3).min(lines.len())].join(" ");
            let retired_context = near.to_ascii_lowercase().contains("retired");
            check_doc_line(line, &at, retired_context, registry, errors);
        }
    }
    Ok(())
}

fn check_doc_line(line: &str, at: &str, retired_context: bool, registry: &SyscallRegistry, errors: &mut Vec<String>) {
    for number in native_literals(line) {
        if !registry.by_name.values().any(|v| *v == number) && !registry.retired.contains(&number) {
            errors.push(format!("{at}: {number:#x} is not a registered or retired native syscall"));
        }
    }
    let mut rest = line;
    while let Some(start) = rest.find("SYS_RUSTOS_") {
        let tail = &rest[start..];
        let len = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..len];
        let after = &tail[len..];
        rest = &tail[len.max(1)..];
        // `SYS_RUSTOS_FOO_{A,B}` and `SYS_RUSTOS_FOO_*_BROKER` are family
        // references, not a single name.
        if name.ends_with('_') {
            continue;
        }
        let Some(&actual) = registry.by_name.get(name) else {
            // A removed syscall may be named in the sentence (or the lines around it) that retires it.
            if retired_context {
                continue;
            }
            errors.push(format!("{at}: {name} is not a declared syscall"));
            continue;
        };
        let after = after.trim_start_matches('`').trim_start();
        let Some(after) = after.strip_prefix('(') else { continue };
        let after = after.trim_start_matches('`');
        if let Some(&claimed) = native_literals(after.split([')', '`']).next().unwrap_or("")).first()
            && after.starts_with("0x5255")
            && claimed != actual
        {
            errors.push(format!("{at}: {name} is {actual:#x}, docs say {claimed:#x}"));
        }
    }
}

/// Every `0x5255_xxxx` (or `0x5255xxxx`) literal on the line, in order.
fn native_literals(line: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("0x5255") {
        let tail = &rest[at + 2..];
        let len = tail
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '_'))
            .unwrap_or(tail.len());
        let digits = tail[..len].trim_end_matches('_').replace('_', "");
        if digits.len() == 8
            && let Ok(number) = u64::from_str_radix(&digits, 16)
        {
            out.push(number);
        }
        rest = &rest[at + 2 + len.max(1)..];
    }
    out
}

/// `const NAME_MARKER: &str = "literal";` with the literal possibly on the next line.
fn parse_markers(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("const ") {
        rest = &rest[at + "const ".len()..];
        let name_len = rest
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(0);
        let name = &rest[..name_len];
        if !name.ends_with("_MARKER") {
            continue;
        }
        let after = rest[name_len..].trim_start();
        let Some(after) = after.strip_prefix(':') else { continue };
        let after = after.trim_start();
        let Some(after) = after.strip_prefix("&str") else { continue };
        let Some(after) = after.trim_start().strip_prefix('=') else { continue };
        let Some(mut body) = after.trim_start().strip_prefix('"') else { continue };
        let mut value = String::new();
        while let Some(c) = body.chars().next() {
            body = &body[c.len_utf8()..];
            match c {
                '"' => break,
                '\\' => {
                    if let Some(escaped) = body.chars().next() {
                        body = &body[escaped.len_utf8()..];
                        value.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    }
                }
                other => value.push(other),
            }
        }
        out.push((name.to_string(), value));
    }
    out
}

fn check_markers(root: &Path, errors: &mut Vec<String>) -> Result<()> {
    let mut markers: Vec<(String, String, PathBuf)> = Vec::new();
    let mut harness: Vec<PathBuf> = vec![root.join("tools/xtask/src/kvm.rs"), root.join("tools/xtask/src/bench.rs")];
    for entry in WalkDir::new(root.join("tools/xtask/src/kvm")) {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "rs") {
            harness.push(entry.path().to_path_buf());
        }
    }
    for file in harness {
        // Unit tests may define fixtures named *_MARKER that nothing prints.
        if file.components().any(|c| c.as_os_str() == "tests") {
            continue;
        }
        let text = fs::read_to_string(&file)?;
        for (name, value) in parse_markers(&text) {
            markers.push((name, value, file.clone()));
        }
    }
    if markers.len() < 20 {
        errors.push(format!("marker scan found only {} *_MARKER constants", markers.len()));
    }
    let sources = collect_marker_sources(root)?;
    for (name, value, file) in markers {
        if MARKER_ALLOWLIST.iter().any(|(allowed, _)| *allowed == name) {
            continue;
        }
        if !marker_present(&sources, &value) {
            errors.push(format!(
                "{} {name} = {value:?} appears in no kernel/service/lib/app source; \
                 the producer was renamed or the probe is stale (add to MARKER_ALLOWLIST only if it is truly external)",
                file.strip_prefix(root).unwrap_or(&file).display()
            ));
        }
    }
    Ok(())
}

/// A marker is the beginning of a formatted line, so a source that formats a
/// value right after it carries only the leading text. A `name=X ` marker is a
/// milestone field, recorded as the string literal `"X"`.
///
/// Sources format values into the line (`generation={}`), while the harness
/// waits for one concrete value (`generation=1`). When the exact text is not
/// present, each fixed piece around a `key=value` is required instead: the text
/// up to and including every `=`, so a renamed phrase or key still fails.
fn marker_present(sources: &str, marker: &str) -> bool {
    if sources.contains(marker) {
        return true;
    }
    if let Some(name) = marker.strip_prefix("name=") {
        return sources.contains(&format!("\"{}\"", name.trim()));
    }
    let pieces = marker_fixed_pieces(marker);
    pieces.first().is_some_and(|piece| piece.len() >= 8) && pieces.iter().all(|piece| sources.contains(piece.as_str()))
}

/// Fixed text of a marker with each `=value` replaced by a cut: for
/// `a b x=1 y=2` the pieces are `a b x=` and ` y=`.
fn marker_fixed_pieces(marker: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut skipping = false;
    for c in marker.chars() {
        if skipping {
            if c == ' ' {
                skipping = false;
                current.push(c);
            }
            continue;
        }
        current.push(c);
        if c == '=' {
            pieces.push(std::mem::take(&mut current));
            skipping = true;
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    pieces
}

fn collect_marker_sources(root: &Path) -> Result<String> {
    let mut all = String::new();
    for dir in ["kernel", "services", "libs", "apps", "driver-domains", "boot"] {
        let base = root.join(dir);
        if !base.is_dir() {
            continue;
        }
        let walker = WalkDir::new(&base).into_iter().filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(name == "target" || name == "node_modules" || name == ".git" || name == "out")
        });
        for entry in walker {
            let entry = entry.with_context(|| format!("walk {}", base.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry.path().extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "rs" | "c" | "h" | "sh" | "S" | "toml") {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    all.push_str(&text);
                    all.push('\n');
                }
            }
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SyscallRegistry {
        let mut by_name = BTreeMap::new();
        by_name.insert("SYS_RUSTOS_FRAME_SOURCE".to_string(), 0x5255_0063);
        by_name.insert("SYS_RUSTOS_APP_BUFFER_BROKER".to_string(), 0x5255_0061);
        SyscallRegistry { by_name, retired: vec![0x5255_0020] }
    }

    fn lint(line: &str) -> Vec<String> {
        let mut errors = Vec::new();
        check_doc_line(line, "t:1", line.contains("retired"), &registry(), &mut errors);
        errors
    }

    #[test]
    fn workspace_has_no_abi_drift() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        check_abi_drift(&root).unwrap();
    }

    #[test]
    fn stale_number_next_to_name_is_reported() {
        assert!(lint("`SYS_RUSTOS_FRAME_SOURCE` (`0x5255_0061`)")[0].contains("docs say 0x52550061"));
        assert!(lint("`SYS_RUSTOS_FRAME_SOURCE` (`0x5255_0063`, ABI v1)").is_empty());
    }

    #[test]
    fn unregistered_and_unknown_are_reported_retired_allowed() {
        assert_eq!(lint("see 0x5255_0099").len(), 1);
        assert!(lint("`0x52550020` is retired").is_empty());
        assert_eq!(lint("`SYS_RUSTOS_NOPE`").len(), 1);
        assert!(lint("`SYS_RUSTOS_FRAME_SOURCE_{A,B}` and SYS_RUSTOS_FOO_*").is_empty());
    }

    #[test]
    fn historical_second_number_is_not_a_claim() {
        assert!(lint("SYS_RUSTOS_FRAME_SOURCE (0x5255_0063; formerly at 0x5255_0061)").is_empty());
    }

    #[test]
    fn markers_parse_across_lines() {
        let found = parse_markers("const A_MARKER: &str =\n    \"a b\";\nconst B: u8 = 1;\nconst C_MARKER: &str = \"c\";");
        assert_eq!(found, vec![("A_MARKER".into(), "a b".into()), ("C_MARKER".into(), "c".into())]);
    }

    #[test]
    fn formatted_values_match_by_fixed_pieces() {
        let source = "\"dvm-block: injected fault operation={} generation={generation}\"";
        assert!(marker_present(source, "dvm-block: injected fault operation=block.flush generation=1"));
        assert!(!marker_present(source, "dvm-block: renamed fault operation=x generation=1"));
        assert!(!marker_present(source, "dvm-block: injected fault operation=x epoch=1"));
    }

    #[test]
    fn marker_lookup_handles_milestone_fields() {
        assert!(marker_present("record(\"clocksource-ready\")", "name=clocksource-ready "));
        assert!(!marker_present("record(\"other\")", "name=clocksource-ready "));
    }
}
