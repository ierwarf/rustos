#!/usr/bin/env python3
from pathlib import Path
import subprocess

BASE_BENCH_COMMIT = "1aa5dff151d52ba7f200827cd5c9614888cbd25b"


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}")
    p.write_text(text.replace(old, new, 1))


# Restore the last clean benchmark workflow, then make expensive benchmarking
# manual-only while boot bring-up is being debugged.
bench = subprocess.check_output(
    ["git", "show", f"{BASE_BENCH_COMMIT}:.github/workflows/bench-run.yml"],
    text=True,
)
old_trigger = '''on:
  workflow_dispatch:
  push:
    branches:
      - "master"
    paths:
      - ".github/workflows/bench-run.yml"
      - "compat/windows/services/winsys/**"
      - "apps/windows/userdemo2/**"
      - "tools/xtask/**"
      - "kernel/**"
      - "libs/**"
      - "services/**"
      - "docs/benchmarks/**"
      - "docs/ai/performance-hardening.md"
'''
new_trigger = '''on:
  workflow_dispatch:
'''
if bench.count(old_trigger) != 1:
    raise SystemExit("bench-run.yml: expected one push trigger block")
Path(".github/workflows/bench-run.yml").write_text(bench.replace(old_trigger, new_trigger, 1))

# LOADER_OP_SPAWN_EXEC has no target namespace identity, so its executable
# path must already be absolute before it crosses into loaderd/vfsd. Keep argv0
# as the caller-facing spelling; bind the scheduling grant and loader request to
# the same canonical executable identity.
replace_once(
    "services/rootd/src/main.rs",
    '''    request.requester_pid = requester_pid as u64;
    request.scheduling_context = register_scheduling_context_authority(
        path,
        request.requester_pid,
        scheduling_context_policy_for_exec(path),
    )
    .map_err(i64::from)?;
    request.flags = SPAWN_FLAG_LOGICAL_ADMIN as u32 | LOADER_SPAWN_FLAG_DEFER_START;
    request.weight_micros = weight_micros;
    request.exec_path_len = path.len() as u32;
    request.argv_count = 1;
    request.argv_bytes_len = (path.len() + 1) as u32;
    copy_bytes(path, &mut request.exec_path);
    copy_bytes(path, &mut request.argv_bytes);
    request.argv_bytes[path.len()] = 0;
''',
    '''    request.requester_pid = requester_pid as u64;
    let mut canonical_path_storage = [0_u8; LOADER_SPAWN_EXEC_PATH_CAPACITY];
    let canonical_path = if path.first() == Some(&b'/') {
        path
    } else {
        let canonical_len = path.len() + 1;
        if canonical_len > canonical_path_storage.len() {
            return Err(22);
        }
        canonical_path_storage[0] = b'/';
        copy_bytes(path, &mut canonical_path_storage[1..]);
        &canonical_path_storage[..canonical_len]
    };
    request.scheduling_context = register_scheduling_context_authority(
        canonical_path,
        request.requester_pid,
        scheduling_context_policy_for_exec(canonical_path),
    )
    .map_err(i64::from)?;
    request.flags = SPAWN_FLAG_LOGICAL_ADMIN as u32 | LOADER_SPAWN_FLAG_DEFER_START;
    request.weight_micros = weight_micros;
    request.exec_path_len = canonical_path.len() as u32;
    request.argv_count = 1;
    request.argv_bytes_len = (path.len() + 1) as u32;
    copy_bytes(canonical_path, &mut request.exec_path);
    copy_bytes(path, &mut request.argv_bytes);
    request.argv_bytes[path.len()] = 0;
''',
)

for path in ("services/initd/src/main.rs", "services/runtimed/src/spawn.rs"):
    replace_once(
        path,
        "    let exec_bytes = exec_path.as_bytes();\n",
        '''    let canonical_exec_path = if exec_path.starts_with('/') {
        exec_path.to_owned()
    } else {
        format!("/{exec_path}")
    };
    let exec_bytes = canonical_exec_path.as_bytes();
''',
    )
    replace_once(
        path,
        "        scheduling_context: request_scheduling_context_authority(exec_path)?,\n",
        "        scheduling_context: request_scheduling_context_authority(canonical_exec_path.as_str())?,\n",
    )

# vfsd returns canonical absolute paths, so post-snapshot UI identity checks
# must compare against that same spelling.
replace_once(
    "services/loaderd/src/main.rs",
    'const UI_SERVER_EXEC_PATH: &str = "services/uiserver/uiserver.elf";',
    'const UI_SERVER_EXEC_PATH: &str = "/services/uiserver/uiserver.elf";',
)
replace_once(
    "services/vfsd/src/main.rs",
    'const UI_SERVER_EXEC_PATH: &[u8] = b"services/uiserver/uiserver.elf";',
    'const UI_SERVER_EXEC_PATH: &[u8] = b"/services/uiserver/uiserver.elf";',
)
