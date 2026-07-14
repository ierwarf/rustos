#!/usr/bin/env bash
# Run every fast, PR-sized RustOS formal model.
set -eo pipefail

models='
boot-volume-admission/BootVolumeAdmission
runtime-control-rpc/RuntimeControlRpc
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
dvm-input-write-deadline/DvmInputWriteDeadline
dvm-input-drain-ownership/DvmInputDrainOwnership
trusted-ui-boundary/TrustedUiBoundary
input-readiness/InputReadiness
ui-frame-budget/UiFrameBudget
ui-input-motion/UiInputMotion
devmgrd-sessiond-isolation/DevmgrdSessiondIsolation
vfio-release-authorization/VfioReleaseAuthorization
driver-domain-fleet/DriverDomainFleet
dvm-display-seqlock/DvmDisplaySeqlock
ipc-reply-deadline/IpcReplyDeadline
scheduler-wakeup/SchedulerWakeup
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
