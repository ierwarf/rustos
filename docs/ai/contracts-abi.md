# AI Contracts — Kernel/Service ABI

IPC service IDs, broker syscalls, handle transfer, and service routing. For package/stage/build/logging: `contracts-infra.md`.
Cross-owner composition is indexed by `system-flows.md` and
`formal/system-flows.tsv`; this file remains the detailed owner/wire contract.

## Kernel/Userspace ABI Surface

See [Core ABI and service registry](abi/core.md#kerneluserspace-abi-surface).

## Boot Initial Task

See [Core ABI and service registry](abi/core.md#boot-initial-task).

## IPC Service Registry

See [Core ABI and service registry](abi/core.md#ipc-service-registry).

## Driver domain contract

See [Driver-domain wire and ownership contract](abi/driver-domain.md#driver-domain-contract).

## Handle Transfer

See [IPC transfer, wait and performance contracts](abi/ipc.md#handle-transfer).

## IPC Wait Discipline

See [IPC transfer, wait and performance contracts](abi/ipc.md#ipc-wait-discipline).

## IPC Performance Acceptance

See [IPC transfer, wait and performance contracts](abi/ipc.md#ipc-performance-acceptance).

## VFS Surface (`vfsd`)

See [VFS and storage contracts](abi/vfs-storage.md#vfs-surface-vfsd).

## Storage Surface (`storaged`)

See [VFS and storage contracts](abi/vfs-storage.md#storage-surface-storaged).

## Input Surface (`inputd`)

See [Input, console and device contracts](abi/input-console-device.md#input-surface-inputd).

## Console / TTY Surface (`runtimed` session policy)

See [Input, console and device contracts](abi/input-console-device.md#console--tty-surface-runtimed-session-policy).

## Device Surface (`devmgrd`)

See [Input, console and device contracts](abi/input-console-device.md#device-surface-devmgrd).

## Process Loader Surface (`loaderd` + `procd`)

See [Loader and scheduling-context contracts](abi/loader-scheduling.md#process-loader-surface-loaderd--procd).

### Process Broker Session

See [Loader and scheduling-context contracts](abi/loader-scheduling.md#process-broker-session).

## Scheduling-context accounting snapshot

See [Loader and scheduling-context contracts](abi/loader-scheduling.md#scheduling-context-accounting-snapshot).

## Network Surface (`netd`)

See [Network, process policy, display and scheduler contracts](abi/policy-display.md#network-surface-netd).

## Process Policy Surface (`procd`)

See [Network, process policy, display and scheduler contracts](abi/policy-display.md#process-policy-surface-procd).

## Syscalld Residual Surface

See [Network, process policy, display and scheduler contracts](abi/policy-display.md#syscalld-residual-surface).

## Commercial-Max Protocol

See [Network, process policy, display and scheduler contracts](abi/policy-display.md#commercial-max-protocol).

## Display Surface

See [Network, process policy, display and scheduler contracts](abi/policy-display.md#display-surface).

## Scheduler

See [Network, process policy, display and scheduler contracts](abi/policy-display.md#scheduler).
