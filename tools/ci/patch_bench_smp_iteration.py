#!/usr/bin/env python3
from pathlib import Path

path = Path("tools/xtask/src/bench.rs")
text = path.read_text()
needle = '''        END_MARKER.to_owned(),
    ];
    if let Some(probe) = isolate_probe {
'''
replacement = '''        END_MARKER.to_owned(),
    ];
    if rustos_vcpus > 1 {
        // Multi-vCPU benchmark runs are iterative performance evidence, not a
        // product/release admission lane. Use the bounded SMP verification
        // profile that kvm-run already uses for iterative multicore boots;
        // otherwise an unsealed tree launches the exhaustive PR verifier
        // before a benchmark and can spend minutes proving unrelated gates.
        kvm_args.push("--smp-iteration".to_owned());
    }
    if let Some(probe) = isolate_probe {
'''
count = text.count(needle)
if count != 1:
    raise SystemExit(f"expected one KVM isolate-probe branch, found {count}")
path.write_text(text.replace(needle, replacement, 1))
