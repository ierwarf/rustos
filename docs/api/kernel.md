# Kernel API

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

This page documents the internal kernel crate API surfaces under `kernel/`.
Most subsystem APIs are exposed through each crate's `src/api.rs`. Prefer these
API modules over reaching into private subsystem modules.

### API Surface Map

| Crate | API module | Purpose |
| --- | --- | --- |
| `kernel-hal` | `kernel/hal/src/api.rs` | GDT/IDT/ACPI/PIC/RTC/SIMD setup, interrupt hooks, CPU helpers. |
| `kernel-mm` | `kernel/mm/src/api.rs` | heap, paging, physical frames, higher-half address helpers. |
| `kernel-object` | `kernel/object/src/api.rs` | typed object/handle/session rights shared by kernel subsystems. |
| `kernel-ipc-runtime` | `kernel/ipc-runtime/src/api.rs` | kernel shared region creation, mapping, frame export. |
| `kernel-ps` | `kernel/ps/src/api.rs` | scheduler, process/thread state, user task lifecycle, waits, snapshots. |
| `kernel-io-manager` | `kernel/io-manager/src/api.rs` | block, boot volume, console, device, tty, input, USB, VFS, drivers. |
| `kernel-compat` | `kernel/compat/src/api.rs` | Linux/Windows compatibility syscall and console host API. |
| `kernel-executive` | `kernel/executive/src/lib.rs` | boot orchestration and cross-subsystem hook registration. |
| `nucleus-core` | `kernel/nucleus-core/src/debug/mod.rs` | structured logging, panic/debug output, boot trace helpers. |

### Boot-Time Call Order

The kernel entry path in `kernel/src/main.rs` uses these APIs in strict order:

```text
hal::disable_interrupts
debug::boot_trace::init
hal::init_gdt
hal::init_idt
mm::init_paging
hal::enter_higher_half
hal::call_with_stack
kernel_executive::boot::kernel_main_bootstrap
```

Do not reorder boot APIs unless the call site and subsystem invariants are
reviewed together.

### HAL API

Use `kernel_hal::api` for architecture and interrupt boundaries.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `boot` | `init_gdt`, `init_idt`, `init_acpi`, `enter_higher_half`, `call_with_stack` | Boot-only setup and unsafe control transfer helpers. |
| `cpu` | `init_simd`, `simd_mode_name`, `current_rip` | CPU feature/runtime helpers. |
| `interrupts` | `disable_interrupts`, `init_pic`, `register_task_hooks`, `register_interrupt_hooks`, `register_heartbeat_hooks` | Hook registration connects HAL IRQ paths to scheduler/IO/heartbeat behavior. |
| `time` | `init_rtc` | RTC initialization and dispatch registration. |

### Memory API

Use `kernel_mm::api` for allocator, paging, physical memory, and virtual address
helpers.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `alloc` | `KernelAllocator`, `init_heap`, `handle_alloc_error` | Kernel global allocator support. |
| `boot` | `init_paging`, `init_phys`, `paging_smoke_test` | Early memory bootstrap. |
| `phys` | `usable_bytes`, `free_bytes`, `alloc_frame`, `free_frame` | Physical frame accounting/allocation. |
| `address_space` | `ProcessAddressSpace`, `PageTableFlags`, `debug_direct_map_flags_for_addr` | Process address space and debug helpers. |
| `virt` | `higher_half_addr`, `kernel_virt_offset` | Higher-half address conversion. |

### Process/Scheduler API

Use `kernel_ps::api` for task scheduling and user process state.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `boot` | `init`, `is_initialized`, `service_deferred_work` | Scheduler lifecycle and deferred work. |
| `fault` | `retire_current_user_task_due_to_fault`, `halt_current_retired_task` | User fault disposition. |
| `process` | `spawn_user_process`, `spawn_kernel_process` | Process creation. |
| `snapshot` | `current_user_snapshot`, `current_user_process_id`, `retain_current_user_process_state`, `with_current_user_process_state_mut` | Current/process state access. |
| `task` | `yield_now`, interrupt handler addresses, console session setters, runtime hooks | Scheduling and interrupt integration. |
| `wait` | `wait_for_child` | Wait/child status handling. |

### IO Manager API

Use `kernel_io_manager::api` for devices, boot volume state, VFS, console, and
driver integration.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `boot` | `init_gui`, `init_boot_info`, `boot_volume_identity`, `enter_kernel_vfs_runtime`, `enter_userspace_runtime` | Boot volume and runtime phase transitions. |
| `block` | `dvm_info`, `submit_dvm_read`, `submit_dvm_write`, `submit_dvm_flush`, `poll_dvm`, `cancel_dvm`, `finish_dvm` | Bounded storage-DVM transport; no physical block registry. |
| `console` | `init_console`, `init_tty`, `write_console`, `service_console` | Kernel console and TTY service. |
| `device` | `open`, `read_to_current_user`, `read_to_user`, `ioctl_from_user` | `/dev`-style device access from kernel/syscall paths. |
| `tty` | session input/output and termios helpers | Console session IO. |
| `driver` | `initialize_loadable_modules_for_class`, Linux runtime hooks | Driver load/service integration. |
| `input` | `init_input`, interrupt handlers, service pending counters | Keyboard/mouse/HID input. |
| `usb` | `init_usb`, debug counters | USB host path. |
| `vfs` | `init_vfs`, `normalize_kernel_path`, mount/open/metadata/access/readlink helpers | Current-process VFS operations. |

### Object And IPC APIs

| Crate | Key APIs | Notes |
| --- | --- | --- |
| `kernel_object::api` | `DeviceHandle`, `DeviceId`, `HandleToken`, rights types, `ConsoleSessionHandle` | Shared object and handle vocabulary. |
| `kernel_ipc_runtime::api` | `create_shared_region`, `map_shared_region`, `shared_region_frames` | Kernel shared memory region lifecycle. |

### Compatibility API

Use `kernel_compat::api` for Linux/Windows compatibility surfaces.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `linux` | Linux process/syscall state re-exports and offset helpers | Linux ABI support. |
| `windows` | Windows compatibility re-exports | Windows userland compatibility. |
| `console_host` | console host helpers | Console-hosted user programs. |
| `syscall` | `init_syscalls`, `service_pending`, GS-base helpers | Syscall and compat runtime service path. |

### Usage Rules

- Import API modules with aliases, for example `use kernel_mm::api as mm_api`.
- Do not call private subsystem modules from another kernel crate when an
  `api.rs` wrapper exists.
- Boot APIs often have ordering assumptions; follow the call order in
  `kernel/src/main.rs` and `kernel/executive`.
- APIs that copy to/from user memory must be called only from syscall or
  process-context-aware paths.
- Use `crate::debug` or `nucleus_core::debug` for kernel logs; see
  [Logging Guide](../logging.md).

<a id="korean"></a>

## 한국어

이 문서는 `kernel/` 아래 internal kernel crate API surface를 설명합니다.
대부분의 subsystem API는 각 crate의 `src/api.rs`를 통해 노출됩니다. 다른
kernel crate에서 private subsystem module을 직접 참조하기보다 이 API module을
우선 사용하세요.

### API Surface Map

| Crate | API module | Purpose |
| --- | --- | --- |
| `kernel-hal` | `kernel/hal/src/api.rs` | GDT/IDT/ACPI/PIC/RTC/SIMD setup, interrupt hook, CPU helper |
| `kernel-mm` | `kernel/mm/src/api.rs` | heap, paging, physical frame, higher-half address helper |
| `kernel-object` | `kernel/object/src/api.rs` | kernel subsystem이 공유하는 typed object/handle/session rights |
| `kernel-ipc-runtime` | `kernel/ipc-runtime/src/api.rs` | kernel shared region 생성, mapping, frame export |
| `kernel-ps` | `kernel/ps/src/api.rs` | scheduler, process/thread state, user task lifecycle, wait, snapshot |
| `kernel-io-manager` | `kernel/io-manager/src/api.rs` | block, boot volume, console, device, tty, input, USB, VFS, driver |
| `kernel-compat` | `kernel/compat/src/api.rs` | Linux/Windows compatibility syscall과 console host API |
| `kernel-executive` | `kernel/executive/src/lib.rs` | boot orchestration과 cross-subsystem hook registration |
| `nucleus-core` | `kernel/nucleus-core/src/debug/mod.rs` | structured logging, panic/debug output, boot trace helper |

### Boot-Time Call Order

`kernel/src/main.rs`의 kernel entry path는 다음 순서로 API를 사용합니다.

```text
hal::disable_interrupts
debug::boot_trace::init
hal::init_gdt
hal::init_idt
mm::init_paging
hal::enter_higher_half
hal::call_with_stack
kernel_executive::boot::kernel_main_bootstrap
```

boot API는 call site와 subsystem invariant를 같이 검토하지 않는 한 순서를
바꾸지 마세요.

### HAL API

architecture와 interrupt boundary는 `kernel_hal::api`를 사용합니다.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `boot` | `init_gdt`, `init_idt`, `init_acpi`, `enter_higher_half`, `call_with_stack` | boot-only setup과 unsafe control transfer helper |
| `cpu` | `init_simd`, `simd_mode_name`, `current_rip` | CPU feature/runtime helper |
| `interrupts` | `disable_interrupts`, `init_pic`, `register_task_hooks`, `register_interrupt_hooks`, `register_heartbeat_hooks` | HAL IRQ path를 scheduler/IO/heartbeat 동작에 연결 |
| `time` | `init_rtc` | RTC initialization과 dispatch registration |

### Memory API

allocator, paging, physical memory, virtual address helper는 `kernel_mm::api`를
사용합니다.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `alloc` | `KernelAllocator`, `init_heap`, `handle_alloc_error` | kernel global allocator support |
| `boot` | `init_paging`, `init_phys`, `paging_smoke_test` | early memory bootstrap |
| `phys` | `usable_bytes`, `free_bytes`, `alloc_frame`, `free_frame` | physical frame accounting/allocation |
| `address_space` | `ProcessAddressSpace`, `PageTableFlags`, `debug_direct_map_flags_for_addr` | process address space와 debug helper |
| `virt` | `higher_half_addr`, `kernel_virt_offset` | higher-half address conversion |

### Process/Scheduler API

task scheduling과 user process state는 `kernel_ps::api`를 사용합니다.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `boot` | `init`, `is_initialized`, `service_deferred_work` | scheduler lifecycle과 deferred work |
| `fault` | `retire_current_user_task_due_to_fault`, `halt_current_retired_task` | user fault disposition |
| `process` | `spawn_user_process`, `spawn_kernel_process` | process creation |
| `snapshot` | `current_user_snapshot`, `current_user_process_id`, `retain_current_user_process_state`, `with_current_user_process_state_mut` | current/process state access |
| `task` | `yield_now`, interrupt handler addresses, console session setters, runtime hooks | scheduling과 interrupt integration |
| `wait` | `wait_for_child` | wait/child status handling |

### IO Manager API

device, boot volume state, VFS, console, driver integration은
`kernel_io_manager::api`를 사용합니다.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `boot` | `init_gui`, `init_boot_info`, `boot_volume_identity`, `enter_kernel_vfs_runtime`, `enter_userspace_runtime` | boot volume과 runtime phase transition |
| `block` | `dvm_info`, `submit_dvm_read`, `submit_dvm_write`, `submit_dvm_flush`, `poll_dvm`, `cancel_dvm`, `finish_dvm` | 제한된 storage-DVM 전송 경계이며 물리 block registry는 없음 |
| `console` | `init_console`, `init_tty`, `write_console`, `service_console` | kernel console과 TTY service |
| `device` | `open`, `read_to_current_user`, `read_to_user`, `ioctl_from_user` | kernel/syscall path의 `/dev` style device access |
| `tty` | session input/output and termios helpers | console session IO |
| `input` | `init`, `service_dvm_input_pending`, `mark_dvm_policy_consumer_ready` | 고정 DVM record copy/wake substrate; framing과 input policy는 inputd 소유 |
| `vfs` | `init_vfs`, `normalize_kernel_path`, mount/open/metadata/access/readlink helpers | current-process VFS operation |

### Object And IPC APIs

| Crate | Key APIs | Notes |
| --- | --- | --- |
| `kernel_object::api` | `DeviceHandle`, `DeviceId`, `HandleToken`, rights types, `ConsoleSessionHandle` | shared object/handle vocabulary |
| `kernel_ipc_runtime::api` | `create_shared_region`, `map_shared_region`, `shared_region_frames` | kernel shared memory region lifecycle |

### Compatibility API

Linux/Windows compatibility surface는 `kernel_compat::api`를 사용합니다.

| Group | Key APIs | Notes |
| --- | --- | --- |
| `linux` | Linux process/syscall state re-exports and offset helpers | Linux ABI support |
| `windows` | Windows compatibility re-exports | Windows userland compatibility |
| `console_host` | console host helpers | console-hosted user program |
| `syscall` | `init_syscalls`, `service_pending`, GS-base helpers | syscall과 compat runtime service path |

### Usage Rules

- `use kernel_mm::api as mm_api`처럼 API module을 alias해서 import하세요.
- `api.rs` wrapper가 있으면 다른 kernel crate에서 private subsystem module을
  직접 호출하지 마세요.
- boot API는 ordering assumption이 강합니다. `kernel/src/main.rs`와
  `kernel/executive`의 call order를 따르세요.
- user memory로 copy하는 API는 syscall 또는 process-context-aware path에서만
  호출해야 합니다.
- kernel log는 `crate::debug` 또는 `nucleus_core::debug`를 사용하세요. 자세한
  내용은 [로깅 가이드](../logging.md)를 참고하세요.
