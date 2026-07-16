# Commercial-model review

This is the commercial-model quality review for every enabled topology. A
passing finite safety check is not sufficient when the model silently assumes
freshness, timer progress, a cleanup transition, or a well-formed
configuration.

## Review rule

Each model was checked for its state type, authority identity, terminal
cleanup, bounded-resource behavior, failure/revoke/exit transition, finite
clock boundary, and—where the contract promises eventual release—an explicit
fairness assumption plus a temporal property. The audit groups and outcome
below cover every model currently invoked by `run-all-tlc.sh`.

| Model group | Models reviewed | Result |
| --- | --- | --- |
| Bootstrap and service lifecycle | `rootd-bootstrap`, `endpoint-registry`, `endpoint-publication`, `deferred-start`, `post-init-leases`, `rootd-restart-backoff`, `post-init-supervisor-recovery` | Added finite-clock admission guards to every wait/backoff/recovery window that could otherwise be created at TLC's final tick. Rootd and endpoint/deferred waits now state timer-fair eventual settlement. Publication and post-init lease identity/cleanup models already contained their rejected and exit transitions. |
| DVM authority and transports | `dvm-control-relay`, `dvm-control-endpoint`, `dvm-network-ring`, `dvm-network-control`, `dvm-input-revocation`, `trusted-ui-boundary`, `gui-dvm-surface`, `dvm-atomic-scanout`, `dvm-commercial-lifecycle`, `dvm-display-driver-supply`, `dvm-amdgpu-supply`, `gui-dvm-install` | Added temporal timeout settlement to control setup/relay. Fresh relay epochs are monotonic and one-shot. Trusted UI includes independent attestation-lease withdrawal. Direct scanout proves fixed triple-slot ownership, no device-write DMA authority, front-slot pinning, page-flip-before-old-front-release, and offline revocation. The commercial lifecycle proves durable pre-exec identity, pre/post group reset, IOMMUFD launch, authenticated readiness, exact-PID recovery, quarantine, disabled physical network/block authority, and rejection of the live L0 display. Physical display supply separately proves exact NVIDIA open-module/GSP identity and exact AMD `1002:1900` signed-module/firmware completeness, kernel modalias selection, relay-after-KMS ordering, and absent compute authority. |
| UI and readiness | `input-readiness`, `ui-frame-budget`, `ui-input-motion`, `devmgrd-sessiond-isolation` | Added fair timer/recheck progress and eventual poll settlement. Frame budget, motion, and devmgrd models already make the blocking worker independent from the UI/main-loop owner, with bounded admission and explicit rejection. |
| Scheduler, clock, and IPC | `ipc-reply-deadline`, `scheduler-wakeup`, `clocksource-deadline`, `scheduler-admission`, `ipc-priority-inheritance`, `ipc-handle-transfer`, `ipc-endpoint-ownership` | Added timer-fair unblocking for reply waits and scheduler arms. Elapsed time is now clocksource-based rather than RTC-edge-counted; delayed PIT delivery catches absolute deadlines, and sleep identity cannot re-enter process policy locks. Runtime-catalog weight is capability-separated from System admission: only the exact UI owner receives its pinned critical weight. System work has an eight-dispatch cap before a ready User turn is mandatory, while reply-scoped inheritance remains transitive and terminally revocable. Added a received-batch owner-exit cleanup path, prohibited reply before transferred-handle installation, and made terminal messages reject detached transfers. |
| Process and hardware authority | `vfio-release-authorization`, `dvm-commercial-lifecycle`, `driver-domain-fleet`, `proc-broker-session`, `exec-ticket` | VFIO explicitly generates and rejects a signed wrong-manifest candidate. The runtime lifecycle adds exact durable process identity and reset/quarantine ordering. Fleet, prepare-session, and exec-ticket models encode signed-fleet exclusivity, owner-exit cleanup, exact PID/TID binding, and sibling/target teardown. |

## Closed model defects

1. Absolute bounded clocks allowed a new wait to be created after its final
   timer transition. The affected models now admit a wait only when its full
   interval fits, and the models that promise release check it temporally.
2. Several timer models proved a deadline field but permitted infinite
   stuttering. Their `Spec` now names the timer/recheck fairness assumption and
   checks the resulting release property.
3. DVM input/network models allowed retired epoch reuse. This could reset a
   sequence counter and make an old sequence valid again. The L0 epoch is now
   allocated atomically and monotonically by hostd, fails closed before wrap,
   and is one-shot in both formal lifecycle models.
4. The VFIO model constructed only a correct manifest and did not compare it
   during authorization. It now explores a signed wrong-manifest release.
5. The transferred-handle models permitted reply before installation and did
   not represent owner death after dequeue. Terminal message states now cannot
   retain received descriptors.
6. Trusted-UI invalidation modeled a lost device but not a still-present
   provider whose independent attestation lease was revoked. That revocation
   now immediately cancels a granted prompt.

The bounded models remain abstractions. They do not prove CPU memory ordering,
ELF/PE parser correctness, DMA behavior, a physical trusted-UI device, or
unbounded host lifetime. Those limits remain documented in `COVERAGE.md` and
require source tests, fuzzing, and bounded KVM validation.
