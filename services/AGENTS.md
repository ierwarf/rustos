# Services Subtree Notes

Inherits the repo root `AGENTS.md`. Add-only overrides below.

## Bootstrap Ordering Traps

### `runtimed` / `bootstrap_ui_server`

The bootstrap UI server **must read the `uiserver` manifest synchronously**
during early bootstrap. The async launch catalog is only fully populated
after `uiserver` itself starts, so an async manifest lookup at bootstrap
time deadlocks: catalog never resolves → uiserver never launches → catalog
never finishes. Keep the synchronous manifest path; do not "modernize" it
to async.

### `uiserver` / `apply_runtime_state`

Do **not** prime `ConsoleWindow` surfaces inside `apply_runtime_state`. The
renderer rebuilds those surfaces from scratch on the next frame anyway, so
priming is wasted work — and worse, it stalls the main loop long enough to
miss the first vsync, producing a visible black frame.

## Service Authority Map

- `syscalld`: Linux MM/VFS/signal/clock policy
- `vfsd`: filesystem namespace and mount table
- `loaderd`: ELF/PE program loading, dynamic linker integration
- `netd`: network stack policy
- `devmgrd`: RustOS device namespace and device lifetime
- Linux DVM: Linux driver enumeration and driver lifetime
- `storaged`: block/partition layer
- `inputd`: HID + input routing
- `procd`: process lifecycle for normal user procs (post-`rootd` handoff)
- `sessiond`: session/login boundary
- `rootd`: kernel-launched first user process, bootstrap authority

Move new policy into the service that owns the domain; do not push it back
into the kernel for "convenience".
