#!/usr/bin/env bash
# Close retired kernel-extension routes and keep device policy in its service.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

fail() {
    echo "kernel policy boundary violation: $*" >&2
    exit 1
}

test ! -e drivers/libs/driver-abi || fail "retired driver-abi crate still exists"
if rg -n 'driver-abi|drivers/libs/driver-abi' Cargo.toml kernel services libs tests \
    --glob '*.toml' --glob '*.rs'; then
    fail "retired driver-abi dependency remains"
fi

if rg -n '\.ko\b' kernel services libs apps \
    --glob '*.rs' --glob '*.c' --glob '*.toml'; then
    fail "RustOS source retains a direct kernel-module route"
fi

retired='(SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER|SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER|SYS_RUSTOS_SERVICE_DRIVER_RESOURCE_BROKER|SYS_RUSTOS_DRIVER_SYMBOL_EVENT_BROKER|IPC_SERVICE_DRIVERD|IPC_SERVICE_SERVICE_DRIVERD|SERVICE_DRIVER_RESOURCE_|LinuxDriverSymbolEventWire|RustosDriverLoadModuleBrokerArgs|RustosDriverProbeAliasBrokerArgs)'
if rg -n "$retired" kernel services libs tests --glob '*.rs' --glob '*.toml'; then
    fail "retired RustOS driver/module ABI is reachable"
fi
test ! -e kernel/compat/src/user/syscall/linux/driver_broker_ops.rs ||
    fail "retired driver broker implementation remains"

for syscall in 0x52550020 0x52550021 0x52550022 0x52550037; do
    rg -q "$syscall" apps/abifuzz/abifuzz.c ||
        fail "abifuzz does not prove retired syscall $syscall fails closed"
done

# RDI1 is a device-specific protocol. Ring0 may copy its fixed 32-byte record
# but must not know magic, CRC, event kinds, key ranges, or session semantics.
if rg -n 'RDI1|KIND_SESSION_START|LINUX_EVDEV_KEY_MAX|crc32\(' \
    kernel/io-manager kernel/compat --glob '*.rs'; then
    fail "DVM input protocol semantics escaped inputd"
fi
for witness in \
    'const MAGIC: \[u8; 4\] = \*b"RDI1"' \
    'struct DvmDecoder' \
    'NETD_IPC_OP_DVM_SESSION'; do
    rg -q "$witness" services/inputd/src/dvm_protocol.rs services/inputd/src/main.rs ||
        fail "inputd DVM policy witness missing: $witness"
done

# inputd has exactly one DVM transport consumer: the dedicated ingestion
# worker. Client STATS/READ/commercial dispatch must never call the broker.
inputd_drain_sites="$(rg -n -F 'drain_transport(' services/inputd/src/main.rs | wc -l)"
[ "$inputd_drain_sites" -eq 2 ] ||
    fail "inputd transport drain escaped the sole worker/function boundary"
if rg -n 'INPUTD_IPC_OP_DRAIN_INGEST|COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST' \
    services/inputd/src/main.rs; then
    fail "retired caller-driven input ingestion route is still implemented"
fi

# No ring3 service may acquire physical device authority. DMA-BUF is a
# service-visible graphics object and intentionally not matched here.
if rg -n '(crate::driver::mmio|map_physical|map_mmio|request_irq|allocate_msi|ServiceDriverMmio|ServiceDriverDma|ServiceDriverIrq|ServiceDriverIoPort)' \
    services --glob '*.rs'; then
    fail "ring3 service contains physical MMIO/IRQ/DMA authority"
fi

if rg -n 'starts_with\("/dev/"\)|== "/dev/|matches!\([^[:cntrl:]]*"/dev' \
    kernel/compat/src --glob '*.rs'; then
    fail "compat classifies the device namespace by path"
fi
rg -q 'VFS_DEVICE_ACCESS_DRM_COMPAT' services/vfsd/src/main.rs ||
    fail "vfsd does not own the explicit DRM compatibility route"

# Service capability selection is a rootd policy decision. Compat may ask for
# the current lease but must not regain a parallel hard-coded role table.
rg -q 'service_capability_via_rootd' kernel/compat/src/user/syscall/linux/ipc_ops.rs ||
    fail "compat no longer resolves service authority through rootd"
rg -q 'fn service_policy_capability' services/rootd/src/main.rs ||
    fail "rootd service capability policy source is missing"
rg -q 'IPC_SERVICE_INPUTD => target == IPC_SERVICE_NETD' services/rootd/src/main.rs ||
    fail "inputd to netd lifecycle dependency is not explicitly least-authority"

# Preserve native executable compatibility while policy stays in loaderd/procd.
for source in \
    services/loaderd/src/elf.rs \
    services/loaderd/src/pe_loader.rs \
    services/loaderd/src/pe_runtime.rs \
    services/loaderd/src/commit.rs \
    services/procd/src/main.rs; do
    test -s "$source" || fail "native ELF/PE policy source missing: $source"
done
rg -q 'PROC_BROKER_FORMAT_ELF64.*PROC_BROKER_FORMAT_PE64' services/loaderd/src/commit.rs ||
    fail "loaderd no longer admits both native ELF64 and PE64"

printf 'kernel policy boundary contracts passed\n'
