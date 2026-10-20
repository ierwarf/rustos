//! Bounded disassembly of the exact archived ELFs, never a source-code proxy.
use crate::Result;
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

const MAX_FUNCTION_BYTES: u64 = 65_536;
const MAX_FUNCTIONS: usize = 24;

#[derive(Debug)]
struct Symbol {
    address: u64,
    bytes: u64,
    name: String,
}

fn parse_symbol(line: &str) -> Option<Symbol> {
    let mut fields = line.split_whitespace();
    let address = u64::from_str_radix(fields.next()?, 16).ok()?;
    let bytes = u64::from_str_radix(fields.next()?, 16).ok()?;
    if !matches!(fields.next()?, "t" | "T") {
        return None;
    }
    let name = fields.collect::<Vec<_>>().join(" ");
    (address != 0 && bytes != 0 && !name.is_empty() && address.checked_add(bytes).is_some())
        .then_some(Symbol {
            address,
            bytes,
            name,
        })
}

fn select_symbols<'a>(symbols: &'a [Symbol], needle: &str) -> Vec<&'a Symbol> {
    // nm order can put several closures before the requested owner. Reserve
    // the first bounded slot for the actual owner when it exists.
    symbols
        .iter()
        .filter(|s| s.name == needle)
        .chain(
            symbols
                .iter()
                .filter(|s| s.name != needle && s.name.starts_with(needle)),
        )
        .take(2)
        .collect()
}

pub(super) fn capture(elf: &Path, directory: &Path, kernel: bool) -> Result<Value> {
    fs::create_dir(directory)?;
    let table = Command::new("timeout")
        .args(["10s", "nm", "-S", "-C", "--defined-only"])
        .arg(elf)
        .output()
        .context("inspect exact component ELF symbols")?;
    ensure!(table.status.success(), "component ELF nm failed");
    let symbols = String::from_utf8_lossy(&table.stdout)
        .lines()
        .filter_map(parse_symbol)
        .collect::<Vec<_>>();
    let needles: &[&str] = if kernel {
        &[
            "syscall_entry",
            "kernel_ps::multitask::irq::commit_fast_ipc_call_handoff_and_yield",
            "kernel_ps::multitask::scheduler::ipc_publication::call",
            "<kernel_ps::multitask::scheduler::dispatch_claim::FundedFrame>::claim",
            "kernel_ps::multitask::scheduler::runqueue::claim::claim_dispatch_inner",
            "<kernel_ps::multitask::scheduler::dispatch_claim::DispatchFrame>::prepare",
            "<kernel_ps::multitask::scheduler::current_return::CurrentReturn>::capture",
            "<kernel_ps::multitask::scheduler::return_architecture::hardware_effect",
        ]
    } else {
        &[
            "ipcbench::measure_stamped::<ipcbench::components::probe_identity::",
            "ipcbench::measure_stamped::<ipcbench::components::probe_clock::",
            "ipcbench::measure_stamped::<ipcbench::components::probe_yield::",
            "ipcbench::measure_stamped::<ipcbench::ipc_probes::probe_ipc_mechanism_with_iterations::",
            "ipcbench::components::probe_identity",
            "ipcbench::components::probe_clock",
            "ipcbench::components::probe_yield",
            "ipcbench::ipc_probes::measure_intra_process_round_trip",
            "ipcbench::main",
        ]
    };
    let mut selected = Vec::new();
    let mut absent = Vec::new();
    for needle in needles {
        let matches = select_symbols(&symbols, needle);
        if matches.is_empty() {
            absent.push(*needle);
        }
        for symbol in matches {
            if selected.len() < MAX_FUNCTIONS
                && !selected
                    .iter()
                    .any(|s: &&Symbol| s.address == symbol.address)
            {
                selected.push(symbol);
            }
        }
    }
    ensure!(
        !selected.is_empty(),
        "no requested linked component functions found"
    );
    if kernel {
        ensure!(
            selected.iter().any(|s| s.name == "syscall_entry"),
            "missing actual syscall entry"
        );
    }
    let mut functions = Vec::new();
    for (index, symbol) in selected.into_iter().enumerate() {
        let bytes = symbol.bytes.min(MAX_FUNCTION_BYTES);
        let output = Command::new("timeout")
            .args([
                "10s",
                "objdump",
                "-d",
                "-C",
                "-Mintel",
                "--no-show-raw-insn",
            ])
            .arg(format!("--start-address={:#x}", symbol.address))
            .arg(format!("--stop-address={:#x}", symbol.address + bytes))
            .arg(elf)
            .output()
            .context("disassemble exact component function")?;
        ensure!(
            output.status.success(),
            "component objdump failed for {}",
            symbol.name
        );
        let file = format!("{index:02}.asm");
        super::components::write_private(&directory.join(&file), &output.stdout)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let instructions = text
            .lines()
            .filter_map(|l| {
                let (address, instruction) = l.split_once(':')?;
                let address = address.trim();
                (!address.is_empty() && address.chars().all(|c| c.is_ascii_hexdigit()))
                    .then_some(instruction.trim())
            })
            .collect::<Vec<_>>();
        functions.push(json!({
            "symbol": symbol.name, "address": symbol.address, "symbol_bytes": symbol.bytes,
            "captured_bytes": bytes, "truncated": bytes != symbol.bytes, "file": file,
            "sha256": super::components::sha256(&output.stdout), "static_only": true,
            "instructions": instructions.len(),
            "callsites": instructions.iter().filter(|s| s.starts_with("call ")).count(),
            "locked_instructions": instructions.iter().filter(|s| s.starts_with("lock ")).count(),
            "rdtsc_sites": instructions.iter().filter(|s| s.starts_with("rdtsc")).count(),
            "wrmsr_sites": instructions.iter().filter(|s| s.starts_with("wrmsr")).count(),
        }));
    }
    Ok(json!({"functions": functions, "absent_or_inlined": absent}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_linked_owner_precedes_its_outlined_closures_in_bounded_capture() {
        let symbols = [
            parse_symbol("100 20 t owner::{closure#0}").unwrap(),
            parse_symbol("200 20 t owner::{closure#1}").unwrap(),
            parse_symbol("300 20 t owner").unwrap(),
        ];
        let chosen = select_symbols(&symbols, "owner");
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0].name, "owner");
        assert!(select_symbols(&symbols, "missing").is_empty());
        let chosen = select_symbols(&symbols[..2], "owner");
        assert_eq!(chosen.len(), 2); // Inlined owner still exposes available code.
    }
    #[test]
    fn component_disassembly_admits_only_bounded_real_code_symbols() {
        let s = parse_symbol("0000000000212038 0000000000000208 T syscall_entry").unwrap();
        assert_eq!(
            (s.address, s.bytes, s.name.as_str()),
            (0x212038, 0x208, "syscall_entry")
        );
        for line in [
            "0 1 T zero",
            "10 0 t empty",
            "ffffffffffffffff 2 T overflow",
            "10 8 D data",
            "10 8 t",
        ] {
            assert!(parse_symbol(line).is_none(), "{line}");
        }
        assert_eq!(
            parse_symbol("10 8 t <a as b>::c").unwrap().name,
            "<a as b>::c"
        );
    }
}
