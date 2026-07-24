-------------------------- MODULE BootStorageHandoff --------------------------
EXTENDS Naturals

(*******************************************************************************
Models the one-way activation and ordered recovery of one physical storage
controller from its host-native driver into one Linux storage DVM.

Concrete production owners:
  * tools/hostd/src/storage.rs
      exclusive whole-device admission, family-wide idle validation, bounded
      fsync + BLKFLSBUF, private generation-bound aperture, revoke
  * tools/hostd/src/main.rs
      signed schema-4 admission, durable lease commit, active-release guard
  * tools/hostd/src/runtime.rs
      gated QEMU launch, exact PID/start-time recovery, authenticated readiness,
      aperture revoke before reset/original-driver restoration
  * libs/driver-domain-host/src/lib.rs
      durable VFIO and storage-handoff binding

RustOS ring0 is not a controller owner in this model. It consumes only the
fixed DVM block aperture after booting from the immutable early-system module.
Host and VFIO authority never overlap. An active DVM must have its exact
recorded process observed exited before the aperture can be revoked, and the
original host driver cannot be restored until that revocation completes.
*******************************************************************************)

CONSTANT MaxGeneration

Phases == {
    "host-active",
    "host-frozen",
    "durable",
    "epoch-bound",
    "vfio-assigned",
    "dvm-launched",
    "dvm-ready",
    "stop-requested",
    "dvm-exited",
    "host-aperture-revoked",
    "vfio-aperture-revoked",
    "host-restored",
    "quarantined"
}

VARIABLES phase, generation, readyGeneration, epochSigned, apertureLive, durable,
          runtimeRecorded, exactPidExited, staleReadyRejected, readOnly,
          revokedReadOnly, revocationRetried, epochIdentityBound

vars == <<phase, generation, readyGeneration, epochSigned, apertureLive, durable,
          runtimeRecorded, exactPidExited, staleReadyRejected, readOnly,
          revokedReadOnly, revocationRetried, epochIdentityBound>>

HostAuthority ==
    phase \in {
        "host-active", "host-frozen", "durable", "epoch-bound", "host-aperture-revoked",
        "host-restored"
    }

VfioAuthority ==
    phase \in {
        "vfio-assigned", "dvm-launched", "dvm-ready", "stop-requested",
        "dvm-exited", "vfio-aperture-revoked", "quarantined"
    }

DvmAuthority == phase \in {"dvm-launched", "dvm-ready", "stop-requested"}

Init ==
    /\ phase = "host-active"
    /\ generation \in 1..MaxGeneration
    /\ readyGeneration = 0
    /\ epochSigned = FALSE
    /\ apertureLive = FALSE
    /\ durable = FALSE
    /\ runtimeRecorded = FALSE
    /\ exactPidExited = TRUE
    /\ staleReadyRejected = FALSE
    /\ readOnly \in BOOLEAN
    /\ revokedReadOnly = FALSE
    /\ revocationRetried = FALSE
    /\ epochIdentityBound = FALSE

FreezeHost ==
    /\ phase = "host-active"
    /\ phase' = "host-frozen"
    /\ apertureLive' = TRUE
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, durable, runtimeRecorded,
                   exactPidExited, staleReadyRejected, readOnly, revokedReadOnly,
                   revocationRetried, epochIdentityBound>>

FlushHost ==
    /\ phase = "host-frozen"
    /\ phase' = "durable"
    /\ durable' = TRUE
    /\ epochSigned' = TRUE
    /\ UNCHANGED <<generation, readyGeneration, apertureLive, runtimeRecorded,
                   exactPidExited, staleReadyRejected, readOnly, revokedReadOnly,
                   revocationRetried, epochIdentityBound>>

BindEpochIdentity ==
    /\ phase = "durable"
    /\ durable
    /\ epochSigned
    /\ phase' = "epoch-bound"
    /\ epochIdentityBound' = TRUE
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   runtimeRecorded, exactPidExited, staleReadyRejected, readOnly,
                   revokedReadOnly, revocationRetried>>

AssignVfio ==
    /\ phase = "epoch-bound"
    /\ durable
    /\ epochSigned
    /\ epochIdentityBound
    /\ phase' = "vfio-assigned"
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   runtimeRecorded, exactPidExited, staleReadyRejected, readOnly,
                   revokedReadOnly, revocationRetried, epochIdentityBound>>

LaunchDvm ==
    /\ phase = "vfio-assigned"
    /\ apertureLive
    /\ phase' = "dvm-launched"
    /\ runtimeRecorded' = TRUE
    /\ exactPidExited' = FALSE
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   staleReadyRejected, readOnly, revokedReadOnly, revocationRetried,
                   epochIdentityBound>>

AdmitDvmReady ==
    /\ phase = "dvm-launched"
    /\ readyGeneration' = generation
    /\ phase' = "dvm-ready"
    /\ UNCHANGED <<generation, epochSigned, apertureLive, durable, runtimeRecorded,
                   exactPidExited, staleReadyRejected, readOnly, revokedReadOnly,
                   revocationRetried, epochIdentityBound>>

RejectStaleReady ==
    /\ phase = "dvm-launched"
    /\ readyGeneration \in 0..MaxGeneration
    /\ readyGeneration # generation
    /\ staleReadyRejected' = TRUE
    /\ UNCHANGED <<phase, generation, readyGeneration, epochSigned, apertureLive, durable,
                   runtimeRecorded, exactPidExited, readOnly, revokedReadOnly,
                   revocationRetried, epochIdentityBound>>

RequestStop ==
    /\ phase \in {"dvm-launched", "dvm-ready"}
    /\ phase' = "stop-requested"
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   runtimeRecorded, exactPidExited, staleReadyRejected, readOnly,
                   revokedReadOnly, revocationRetried, epochIdentityBound>>

ObserveExactExit ==
    /\ phase = "stop-requested"
    /\ runtimeRecorded
    /\ phase' = "dvm-exited"
    /\ exactPidExited' = TRUE
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   runtimeRecorded, staleReadyRejected, readOnly, revokedReadOnly,
                   revocationRetried, epochIdentityBound>>

AbortBeforeVfio ==
    /\ phase \in {"host-frozen", "durable", "epoch-bound"}
    /\ phase' = "host-aperture-revoked"
    /\ apertureLive' = FALSE
    /\ revokedReadOnly' = readOnly
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, durable, runtimeRecorded,
                   exactPidExited, staleReadyRejected, readOnly, revocationRetried,
                   epochIdentityBound>>

RecoverBeforeLaunch ==
    /\ phase = "vfio-assigned"
    /\ phase' = "dvm-exited"
    /\ exactPidExited' = TRUE
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   runtimeRecorded, staleReadyRejected, readOnly, revokedReadOnly,
                   revocationRetried, epochIdentityBound>>

RevokeAperture ==
    /\ phase = "dvm-exited"
    /\ exactPidExited
    /\ phase' = "vfio-aperture-revoked"
    /\ apertureLive' = FALSE
    /\ revokedReadOnly' = readOnly
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, durable, runtimeRecorded,
                   exactPidExited, staleReadyRejected, readOnly, revocationRetried,
                   epochIdentityBound>>

RetryRevocation ==
    /\ phase \in {"host-aperture-revoked", "vfio-aperture-revoked"}
    /\ revokedReadOnly = readOnly
    /\ revocationRetried' = TRUE
    /\ UNCHANGED <<phase, generation, readyGeneration, epochSigned, apertureLive,
                   durable, runtimeRecorded, exactPidExited, staleReadyRejected,
                   readOnly, revokedReadOnly, epochIdentityBound>>

RestoreHost ==
    /\ phase \in {"host-aperture-revoked", "vfio-aperture-revoked"}
    /\ ~apertureLive
    /\ phase' = "host-restored"
    /\ runtimeRecorded' = FALSE
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   exactPidExited, staleReadyRejected, readOnly, revokedReadOnly,
                   revocationRetried, epochIdentityBound>>

Quarantine ==
    /\ phase \in {"vfio-assigned", "dvm-exited"}
    /\ phase' = "quarantined"
    /\ UNCHANGED <<generation, readyGeneration, epochSigned, apertureLive, durable,
                   runtimeRecorded, exactPidExited, staleReadyRejected, readOnly,
                   revokedReadOnly, revocationRetried, epochIdentityBound>>

Next ==
    FreezeHost
    \/ FlushHost
    \/ BindEpochIdentity
    \/ AssignVfio
    \/ LaunchDvm
    \/ AdmitDvmReady
    \/ RejectStaleReady
    \/ RequestStop
    \/ ObserveExactExit
    \/ AbortBeforeVfio
    \/ RecoverBeforeLaunch
    \/ RevokeAperture
    \/ RetryRevocation
    \/ RestoreHost
    \/ Quarantine

TypeOK ==
    /\ phase \in Phases
    /\ generation \in 1..MaxGeneration
    /\ readyGeneration \in 0..MaxGeneration
    /\ epochSigned \in BOOLEAN
    /\ apertureLive \in BOOLEAN
    /\ durable \in BOOLEAN
    /\ runtimeRecorded \in BOOLEAN
    /\ exactPidExited \in BOOLEAN
    /\ staleReadyRejected \in BOOLEAN
    /\ readOnly \in BOOLEAN
    /\ revokedReadOnly \in BOOLEAN
    /\ revocationRetried \in BOOLEAN
    /\ epochIdentityBound \in BOOLEAN

ControllerAuthorityIsExclusive == ~(HostAuthority /\ VfioAuthority)
DvmRequiresVfio == DvmAuthority => VfioAuthority
VfioRequiresDurability == VfioAuthority => durable
VfioRequiresEpochIdentity == VfioAuthority => epochIdentityBound
DvmRequiresAperture == DvmAuthority => apertureLive
DvmRequiresRuntimeRecord == DvmAuthority => runtimeRecorded
DvmRequiresSignedEpoch == DvmAuthority => epochSigned
ReadyBindsCurrentGeneration ==
    phase = "dvm-ready" => readyGeneration = generation
RevocationRequiresExactExit ==
    phase \in {
        "host-aperture-revoked", "vfio-aperture-revoked", "host-restored"
    } => exactPidExited
RestoreRequiresRevocation ==
    phase = "host-restored" => ~apertureLive /\ ~DvmAuthority
QuarantineNeverRestoresHost == phase = "quarantined" => ~HostAuthority
SignedStaticFlagsSurviveRevocation ==
    phase \in {
        "host-aperture-revoked", "vfio-aperture-revoked", "host-restored"
    } => revokedReadOnly = readOnly
RetryUsesTheSameSignedStaticFlags ==
    revocationRetried => revokedReadOnly = readOnly

Spec == Init /\ [][Next]_vars
===============================================================================
