#!/usr/bin/env bash
# Run every fast, PR-sized RustOS formal model.
set -eo pipefail

models='
rootd-bootstrap/RootdBootstrap
endpoint-registry/EndpointRegistry
endpoint-publication/EndpointPublication
deferred-start/DeferredStart
post-init-leases/PostInitLeases
dvm-control-relay/DvmControlRelay
dvm-display-seqlock/DvmDisplaySeqlock
ipc-reply-deadline/IpcReplyDeadline
scheduler-wakeup/SchedulerWakeup
ipc-handle-transfer/IpcHandleTransfer
ipc-endpoint-ownership/IpcEndpointOwnership
proc-broker-session/ProcBrokerSession
exec-ticket/ExecTicket
'

for model in $models; do
    echo "== TLC: $model =="
    bash formal/run-tlc.sh "$model"
done
