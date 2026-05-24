# Ring3 Migration Inventory

Use this after `docs/ai/ring3-evacuation.md` when a migration batch needs a
current LOC/owner/action snapshot before code deletion.

Regenerate the live inventory with:

```bash
cargo xtask ring3-inventory
```

Current snapshot:

- `total_marked_loc=20449`
- `excluded_xhci_nvme_loc=2910`
- `active_batch_marked_loc=17539`

`kernel/io-manager/src/usb/xhci.rs` and
`kernel/io-manager/src/storage/nvme.rs` are explicitly excluded from the active
batch while `.ko` replacement is being evaluated.

The previous snapshot (active=18512) was reduced by retiring the
`usb/synthetic.rs` marker entirely: the file was deleted along with its dead
`emulation`/`core`/`runtime` plumbing after confirming `synthetic_hid_kind` was
never `Some(..)` post the 2026-05-20 capture-bridge removal.

The latest inputd shrink moved USB HID keyboard key-state diffing, pointer
button-edge state, and reader-visible event injection into `inputd`; ring0 now
forwards normalized HID keyboard/pointer reports through the input ingress
broker while HID descriptor/layout parsing remains in `usb/runtime.rs`.

## Active Batch Lanes

| LOC | Lane | Owner | Action | Path |
| ---: | --- | --- | --- | --- |
| 1297 | abi-first-large | loaderd-procd | move cold image/process policy into loaderd/procd before deleting ring0 parser branches | `kernel/compat/src/user/process/linux.rs` |
| 1210 | abi-first-large | loaderd-procd | move cold image/process policy into loaderd/procd before deleting ring0 parser branches | `kernel/compat/src/user/syscall/linux/proc_broker_ops.rs` |
| 1195 | abi-first-large | uiserver | move provider/display policy into uiserver or service-driver path | `kernel/io-manager/src/driver/virtio_gpu.rs` |
| 1063 | abi-first-large | rootd-capability | move namespace/capability policy behind rootd capability protocol | `kernel/compat/src/user/syscall/linux/ipc_ops.rs` |
| 999 | service-shrink | inputd | move legacy input routing into inputd service-driver path | `kernel/io-manager/src/driver/serio.rs` |
| 956 | service-shrink | netd | replace marked policy with service-owned protocol and then remove marker | `kernel/ps/src/user/socket.rs` |
| 785 | service-shrink | uiserver | move provider/display policy into uiserver or service-driver path | `kernel/io-manager/src/io/gui/framebuffer.rs` |
| 770 | service-shrink | inputd | move legacy input routing into inputd service-driver path | `kernel/io-manager/src/input/i8042.rs` |
| 768 | service-shrink | inputd | move HID parse/state policy into inputd, keep USB callback source | `kernel/io-manager/src/usb/runtime.rs` |
| 728 | service-shrink | storaged | move post-bootstrap storage policy into storaged, keep raw block broker | `kernel/io-manager/src/storage/ahci.rs` |
| 622 | service-shrink | netd | replace marked policy with service-owned protocol and then remove marker | `kernel/compat/src/user/syscall/linux/net_broker_ops.rs` |
| 589 | service-shrink | uiserver | move provider/display policy into uiserver or service-driver path | `kernel/io-manager/src/io/gui.rs` |
| 565 | service-shrink | inputd | replace marked policy with service-owned protocol and then remove marker | `kernel/io-manager/src/usb/core.rs` |
| 550 | policy-bridge | devmgrd | replace marked policy with service-owned protocol and then remove marker | `kernel/compat/src/user/sysops/device.rs` |
| 529 | policy-bridge | sessiond | replace marked policy with service-owned protocol and then remove marker | `kernel/io-manager/src/io/tty.rs` |
| 495 | policy-bridge | syscalld-pagerd | replace marked policy with service-owned protocol and then remove marker | `kernel/compat/src/user/syscall/linux/mm_broker_ops.rs` |
| 483 | service-shrink | storaged | move post-bootstrap storage policy into storaged, keep raw block broker | `kernel/io-manager/src/storage/boot_volume.rs` |
| 478 | service-shrink | uiserver | move provider/display policy into uiserver or service-driver path | `kernel/io-manager/src/io/gui/backend.rs` |
| 477 | service-shrink | uiserver | move provider/display policy into uiserver or service-driver path | `kernel/io-manager/src/io/gui/terminal.rs` |
| 462 | policy-bridge | vfsd-pagerd | replace marked policy with service-owned protocol and then remove marker | `kernel/compat/src/user/sysops/file.rs` |
| 422 | policy-bridge | syscalld-pagerd | replace marked policy with service-owned protocol and then remove marker | `kernel/compat/src/user/syscall/linux/memory_ops.rs` |
| 371 | policy-bridge | syscalld-pagerd | replace marked policy with service-owned protocol and then remove marker | `kernel/compat/src/user/syscall/linux/syscalld_ops.rs` |
| 343 | policy-bridge | devmgrd | replace marked policy with service-owned protocol and then remove marker | `kernel/io-manager/src/io/device/display.rs` |
| 324 | service-shrink | sessiond | replace marked policy with service-owned protocol and then remove marker | `kernel/io-manager/src/io/session.rs` |
| 248 | policy-bridge | syscalld-pagerd | replace marked policy with service-owned protocol and then remove marker | `kernel/compat/src/user/sysops/win32/memory.rs` |
| 247 | service-shrink | storaged | move post-bootstrap storage policy into storaged, keep raw block broker | `kernel/io-manager/src/storage/block/boot.rs` |
| 239 | service-shrink | storaged | move post-bootstrap storage policy into storaged, keep raw block broker | `kernel/io-manager/src/storage/block/io.rs` |
| 227 | service-shrink | sessiond | replace marked policy with service-owned protocol and then remove marker | `kernel/io-manager/src/io/console.rs` |
| 97 | abi-first-large | loaderd-procd | move cold image/process policy into loaderd/procd before deleting ring0 parser branches | `kernel/compat/src/user/process/mod.rs` |

## Excluded `.ko` Evaluation Lane

| LOC | Lane | Owner | Action | Path |
| ---: | --- | --- | --- | --- |
| 2125 | exclude-ko-eval | usb-ko-eval | hold for `.ko` replacement decision | `kernel/io-manager/src/usb/xhci.rs` |
| 785 | exclude-ko-eval | storage-ko-eval | hold for `.ko` replacement decision | `kernel/io-manager/src/storage/nvme.rs` |

## Required Validation

Use this ladder before claiming a large chunk is complete:

```bash
cargo xtask ring3-inventory
cargo xtask check
cargo xtask build
cargo xtask run --profile nvme --accel-profile kvm --usb-input --debugcon file --commercial-max-ready -- --no-reboot
```
