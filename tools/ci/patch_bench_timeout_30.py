#!/usr/bin/env python3
from pathlib import Path

path = Path('tools/xtask/src/bench.rs')
text = path.read_text()
old = '''const fn benchmark_timeout_seconds(rustos_vcpus: u8, isolated: bool) -> u64 {
    if rustos_vcpus > 1 && isolated {
        90
    } else if rustos_vcpus > 1 || isolated {
        60
    } else {
        30
    }
}
'''
new = '''const fn benchmark_timeout_seconds(rustos_vcpus: u8, isolated: bool) -> u64 {
    if rustos_vcpus > 1 {
        30
    } else if isolated {
        60
    } else {
        30
    }
}
'''
if text.count(old) != 1:
    raise SystemExit('benchmark timeout function changed unexpectedly')
path.write_text(text.replace(old, new, 1))
