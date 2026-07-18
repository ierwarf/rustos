#!/usr/bin/env bash
# Run every fast, PR-sized RustOS formal model.
set -eo pipefail

models='
boot-volume-admission/BootVolumeAdmission
runtime-control-rpc/RuntimeControlRpc
dual-abi-image-admission/DualAbiImageAdmission
dual-abi-byte-parser/DualAbiByteParser
page-table-lifecycle/PageTableLifecycle
dma-iommu-isolation/DmaIommuIsolation
filesystem-content-integrity/FilesystemContentIntegrity
network-payload-session/NetworkPayloadSession
scheduler-cpu-distribution/SchedulerCpuDistribution
rootd-bootstrap/RootdBootstrap
endpoint-registry/EndpointRegistry
endpoint-publication/EndpointPublication
deferred-start/DeferredStart
post-init-leases/PostInitLeases
rootd-restart-backoff/RootdRestartBackoff
post-init-supervisor-recovery/PostInitSupervisorRecovery
dvm-control-relay/DvmControlRelay
dvm-control-endpoint/DvmControlEndpoint
dvm-network-ring/DvmNetworkRing
dvm-network-control/DvmNetworkControl
dvm-input-revocation/DvmInputRevocation
dvm-input-ring/DvmInputRing
trusted-ui-boundary/TrustedUiBoundary
input-readiness/InputReadiness
input-ingestion-worker/InputIngestionWorker
ui-frame-budget/UiFrameBudget
wayland-accept-isolation/WaylandAcceptIsolation
ui-input-motion/UiInputMotion
dvm-input-selftest/DvmInputSelftest
dvm-absolute-pointer/DvmAbsolutePointer
devmgrd-sessiond-isolation/DevmgrdSessiondIsolation
vfio-release-authorization/VfioReleaseAuthorization
driver-domain-fleet/DriverDomainFleet
ivshmem-pairing/IvshmemPairing
gui-dvm-surface/GuiDvmSurface
dvm-atomic-scanout/DvmAtomicScanout
dvm-gpu-compositor/DvmGpuCompositor
dvm-display-scheduler/DvmDisplayScheduler
dvm-gpu-admission/DvmGpuAdmission
dvm-gpu-atlas-transport/DvmGpuAtlasTransport
dvm-commercial-lifecycle/DvmCommercialLifecycle
dvm-release-bundle/DvmReleaseBundle
dvm-display-driver-supply/DvmDisplayDriverSupply
dvm-amdgpu-supply/DvmAmdgpuSupply
dvm-amdgpu-evidence/DvmAmdgpuEvidence
gui-dvm-pixel-authority/GuiDvmPixelAuthority
gui-dvm-install/GuiDvmInstall
ipc-reply-deadline/IpcReplyDeadline
scheduler-wakeup/SchedulerWakeup
scheduler-thread-demotion/SchedulerThreadDemotion
clocksource-deadline/ClocksourceDeadline
scheduler-admission/SchedulerAdmission
ipc-priority-inheritance/IpcPriorityInheritance
ipc-handle-transfer/IpcHandleTransfer
ipc-endpoint-ownership/IpcEndpointOwnership
proc-broker-session/ProcBrokerSession
exec-ticket/ExecTicket
'

for model in $models; do
    echo "== TLC: $model =="
    bash formal/run-tlc.sh "$model"
done
