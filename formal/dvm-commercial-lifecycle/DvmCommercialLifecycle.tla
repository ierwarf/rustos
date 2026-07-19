---------------------- MODULE DvmCommercialLifecycle -----------------------
EXTENDS Naturals

(*******************************************************************************
Commercial physical-device DVM lifecycle contract.

Concrete owners:
  tools/hostd/src/runtime.rs
  libs/driver-domain-host/src/lib.rs

This model begins after signed release authorization has admitted one complete
IOMMU group.  Binding first requires the reversible physical-runtime preflight:
trusted exact QEMU/policy/artifacts, a usable IOMMUFD, sufficient bounded QEMU
memlock/pinning budget, and live sysfs evidence
that the group is neither the L0 boot display nor driving a connected DRM
connector.  `dmaBindSafe` includes both vfio-pci idle-D3/bus-master quiescence
and the repeated 4 GiB memlock admission needed before reset/launch. Every
enabled reset method must also have an impact scope contained
by the admitted IOMMU group; a bus-reset fallback is legal only when the full
affected bus belongs to the same lease.  An AMD launch also requires an exact
VFCT identity match, validated ATOM VBIOS, and an owner-private snapshot that
QEMU receives before device execution.  Launch then requires
a durable active lease, every group member bound
to VFIO, a successful pre-launch reset, an IOMMUFD-backed non-identity address
space, and a private runtime record.  Readiness additionally requires an
authenticated control exchange whose inventory proves a supported DRM driver
and the live evidence-v2 DMA-BUF/GPU/fence/atomic-KMS relay lock. Normal
restoration is legal only after the
exact recorded process has stopped and the whole group has reset again.  Any
reset failure retains VFIO ownership and a durable quarantine record.
An unsuccessful spontaneous or requested child exit may still be reset and
restored safely, but the supervisor must never accept it as a successful run.

Signed inputs are admitted only after their canonical files, owning directory
chain, executable, and detached-signature verifier are non-mutable by another
uid.  Crash recovery binds the checked PID/start token to a pidfd before it can
signal; a reused numeric PID is rejected and never receives a signal. Normal
host-requested shutdown first negotiates QMP capabilities, requests ACPI
powerdown, and waits for the exact QEMU process to exit. Command acceptance
alone is never exit evidence. TERM/KILL are bounded recovery fallbacks and a
forced stop can never be accepted as a successful supervised run.

Physical network/block assignment and trusted multi-DVM UI switching are
deliberately disabled in this product slice; enabling either is outside this
model rather than an implicit fallback.
*******************************************************************************)

Idle == "idle"
Prepared == "prepared"
Starting == "starting"
Ready == "ready"
Stopping == "stopping"
Stopped == "stopped"
Restored == "restored"
Quarantined == "quarantined"

VARIABLES phase,
          durableLease,
          vfioBound,
          originalDriverBound,
          preLaunchReset,
          postStopReset,
          iommuFd,
          nonIdentityMap,
          runtimeRecord,
          childAlive,
          pidTokenMatches,
          controlAuthenticated,
          networkAssigned,
          blockAssigned,
          resetFailed,
          recoveryRejected,
          launchInputsTrusted,
          launchRejected,
          pidfdIdentityBound,
          runtimePreflightPassed,
          hostDisplayActive,
          displaySafetyPassed,
          resetScopeSafe,
          resetSafetyPassed,
          dmaBindSafe,
          dmaSafetyPassed,
          vbiosSupplySafe,
          vbiosSupplyPassed,
          childExitFailed,
          supervisorAcceptedExit,
          qmpPowerdownAttempted,
          qemuExitObserved,
          forcedStopUsed

vars == <<phase, durableLease, vfioBound, originalDriverBound,
          preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
          runtimeRecord, childAlive, pidTokenMatches, controlAuthenticated,
          networkAssigned, blockAssigned, resetFailed, recoveryRejected,
          launchInputsTrusted, launchRejected, pidfdIdentityBound,
          runtimePreflightPassed, hostDisplayActive, displaySafetyPassed,
          resetScopeSafe, resetSafetyPassed,
          dmaBindSafe, dmaSafetyPassed,
          vbiosSupplySafe, vbiosSupplyPassed,
          childExitFailed, supervisorAcceptedExit, qmpPowerdownAttempted,
          qemuExitObserved, forcedStopUsed>>

outcomeVars == <<childExitFailed, supervisorAcceptedExit,
                 qmpPowerdownAttempted, qemuExitObserved, forcedStopUsed>>
admissionVars == <<runtimePreflightPassed, hostDisplayActive,
                   displaySafetyPassed, resetScopeSafe, resetSafetyPassed,
                   dmaBindSafe, dmaSafetyPassed, vbiosSupplySafe,
                   vbiosSupplyPassed>>
stableAdmissionVars == <<outcomeVars, admissionVars>>

Init ==
    /\ phase = Idle
    /\ durableLease = FALSE
    /\ vfioBound = FALSE
    /\ originalDriverBound = TRUE
    /\ preLaunchReset = FALSE
    /\ postStopReset = FALSE
    /\ iommuFd = FALSE
    /\ nonIdentityMap = FALSE
    /\ runtimeRecord = FALSE
    /\ childAlive = FALSE
    /\ pidTokenMatches = FALSE
    /\ controlAuthenticated = FALSE
    /\ networkAssigned = FALSE
    /\ blockAssigned = FALSE
    /\ resetFailed = FALSE
    /\ recoveryRejected = FALSE
    /\ launchInputsTrusted = FALSE
    /\ launchRejected = FALSE
    /\ pidfdIdentityBound = FALSE
    /\ runtimePreflightPassed = FALSE
    /\ hostDisplayActive \in BOOLEAN
    /\ displaySafetyPassed = FALSE
    /\ resetScopeSafe \in BOOLEAN
    /\ resetSafetyPassed = FALSE
    /\ dmaBindSafe \in BOOLEAN
    /\ dmaSafetyPassed = FALSE
    /\ vbiosSupplySafe \in BOOLEAN
    /\ vbiosSupplyPassed = FALSE
    /\ childExitFailed = FALSE
    /\ supervisorAcceptedExit = FALSE
    /\ qmpPowerdownAttempted = FALSE
    /\ qemuExitObserved = FALSE
    /\ forcedStopUsed = FALSE

VerifyTrustedLaunchInputs ==
    /\ phase = Idle
    /\ ~launchInputsTrusted
    /\ ~launchRejected
    /\ launchInputsTrusted' = TRUE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchRejected,
                  pidfdIdentityBound>>

RejectMutableLaunchInputs ==
    /\ phase = Idle
    /\ ~launchInputsTrusted
    /\ launchRejected' = TRUE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  pidfdIdentityBound>>

VerifyPhysicalRuntimePreflight ==
    /\ phase = Idle
    /\ launchInputsTrusted
    /\ ~launchRejected
    /\ ~runtimePreflightPassed
    /\ ~hostDisplayActive
    /\ resetScopeSafe
    /\ dmaBindSafe
    /\ vbiosSupplySafe
    /\ runtimePreflightPassed' = TRUE
    /\ displaySafetyPassed' = TRUE
    /\ resetSafetyPassed' = TRUE
    /\ dmaSafetyPassed' = TRUE
    /\ vbiosSupplyPassed' = TRUE
    /\ UNCHANGED <<hostDisplayActive, resetScopeSafe, dmaBindSafe,
                  vbiosSupplySafe>>
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound>>

RejectUnsafeResetScope ==
    /\ phase = Idle
    /\ launchInputsTrusted
    /\ ~resetScopeSafe
    /\ ~launchRejected
    /\ launchRejected' = TRUE
    /\ runtimePreflightPassed' = FALSE
    /\ resetSafetyPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  pidfdIdentityBound, hostDisplayActive,
                  displaySafetyPassed, resetScopeSafe, dmaBindSafe,
                  dmaSafetyPassed, vbiosSupplySafe, vbiosSupplyPassed>>

RejectUnsafeDmaBind ==
    /\ phase = Idle
    /\ launchInputsTrusted
    /\ ~dmaBindSafe
    /\ ~launchRejected
    /\ launchRejected' = TRUE
    /\ runtimePreflightPassed' = FALSE
    /\ dmaSafetyPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  pidfdIdentityBound, hostDisplayActive,
                  displaySafetyPassed, resetScopeSafe, resetSafetyPassed,
                  dmaBindSafe, vbiosSupplySafe, vbiosSupplyPassed>>

RejectActiveHostDisplay ==
    /\ phase = Idle
    /\ launchInputsTrusted
    /\ hostDisplayActive
    /\ ~launchRejected
    /\ launchRejected' = TRUE
    /\ runtimePreflightPassed' = FALSE
    /\ displaySafetyPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  pidfdIdentityBound, hostDisplayActive, resetScopeSafe,
                  resetSafetyPassed, dmaBindSafe, dmaSafetyPassed,
                  vbiosSupplySafe, vbiosSupplyPassed>>

HostDisplayBecomesActiveBeforeBind ==
    /\ phase = Idle
    /\ runtimePreflightPassed
    /\ displaySafetyPassed
    /\ ~hostDisplayActive
    /\ hostDisplayActive' = TRUE
    /\ displaySafetyPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound, resetScopeSafe,
                  resetSafetyPassed, dmaBindSafe, dmaSafetyPassed,
                  vbiosSupplySafe, vbiosSupplyPassed>>

ResetScopeBecomesUnsafeBeforeBind ==
    /\ phase = Idle
    /\ runtimePreflightPassed
    /\ resetSafetyPassed
    /\ resetScopeSafe
    /\ resetScopeSafe' = FALSE
    /\ resetSafetyPassed' = FALSE
    /\ runtimePreflightPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound, hostDisplayActive,
                  displaySafetyPassed, dmaBindSafe, dmaSafetyPassed,
                  vbiosSupplySafe, vbiosSupplyPassed>>

DmaBindBecomesUnsafeBeforeBind ==
    /\ phase = Idle
    /\ runtimePreflightPassed
    /\ dmaSafetyPassed
    /\ dmaBindSafe
    /\ dmaBindSafe' = FALSE
    /\ dmaSafetyPassed' = FALSE
    /\ runtimePreflightPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound, hostDisplayActive,
                  displaySafetyPassed, resetScopeSafe, resetSafetyPassed,
                  vbiosSupplySafe, vbiosSupplyPassed>>

RejectUnsafeVbiosSupply ==
    /\ phase = Idle
    /\ launchInputsTrusted
    /\ ~vbiosSupplySafe
    /\ ~launchRejected
    /\ launchRejected' = TRUE
    /\ runtimePreflightPassed' = FALSE
    /\ vbiosSupplyPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  pidfdIdentityBound, hostDisplayActive,
                  displaySafetyPassed, resetScopeSafe, resetSafetyPassed,
                  dmaBindSafe, dmaSafetyPassed, vbiosSupplySafe>>

VbiosSupplyBecomesUnsafeBeforeBind ==
    /\ phase = Idle
    /\ runtimePreflightPassed
    /\ vbiosSupplyPassed
    /\ vbiosSupplySafe
    /\ vbiosSupplySafe' = FALSE
    /\ vbiosSupplyPassed' = FALSE
    /\ runtimePreflightPassed' = FALSE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound, hostDisplayActive,
                  displaySafetyPassed, resetScopeSafe, resetSafetyPassed,
                  dmaBindSafe, dmaSafetyPassed>>

PrepareAuthorizedGroup ==
    /\ phase = Idle
    /\ launchInputsTrusted
    /\ runtimePreflightPassed
    /\ displaySafetyPassed
    /\ ~hostDisplayActive
    /\ resetSafetyPassed
    /\ resetScopeSafe
    /\ dmaSafetyPassed
    /\ dmaBindSafe
    /\ vbiosSupplyPassed
    /\ vbiosSupplySafe
    /\ phase' = Prepared
    /\ durableLease' = TRUE
    /\ vfioBound' = TRUE
    /\ originalDriverBound' = FALSE
    /\ UNCHANGED <<preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound>>

ResetBeforeLaunch ==
    /\ phase = Prepared
    /\ vfioBound
    /\ durableLease
    /\ resetSafetyPassed
    /\ resetScopeSafe
    /\ preLaunchReset' = TRUE
    /\ UNCHANGED <<phase, vfioBound, originalDriverBound, durableLease,
                  postStopReset, iommuFd, nonIdentityMap, runtimeRecord,
                  childAlive, pidTokenMatches, controlAuthenticated,
                  networkAssigned, blockAssigned, resetFailed,
                  recoveryRejected, launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

LaunchWithIommuFd ==
    /\ phase = Prepared
    /\ durableLease
    /\ vfioBound
    /\ preLaunchReset
    /\ phase' = Starting
    /\ iommuFd' = TRUE
    /\ nonIdentityMap' = TRUE
    /\ runtimeRecord' = TRUE
    /\ childAlive' = TRUE
    /\ pidTokenMatches' = TRUE
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, controlAuthenticated,
                  networkAssigned, blockAssigned, resetFailed,
                  recoveryRejected, launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

AuthenticateControlAndDisplay ==
    /\ phase = Starting
    /\ childAlive
    /\ runtimeRecord
    /\ phase' = Ready
    /\ controlAuthenticated' = TRUE
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  networkAssigned, blockAssigned, resetFailed,
                  recoveryRejected, launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

RequestStop ==
    /\ phase \in {Starting, Ready}
    /\ childAlive
    /\ phase' = Stopping
    /\ qmpPowerdownAttempted' = TRUE
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound>>

LoseAuthenticatedHealth ==
    /\ phase = Ready
    /\ childAlive
    /\ phase' = Stopping
    /\ controlAuthenticated' = FALSE
    /\ qmpPowerdownAttempted' = TRUE
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  networkAssigned, blockAssigned, resetFailed,
                  recoveryRejected, launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

ObserveExactChildExitBase ==
    /\ phase \in {Ready, Stopping}
    /\ childAlive
    /\ pidTokenMatches
    /\ ~pidfdIdentityBound
    /\ phase' = Stopped
    /\ childAlive' = FALSE
    /\ iommuFd' = FALSE
    /\ nonIdentityMap' = FALSE
    /\ qemuExitObserved' = TRUE
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, runtimeRecord,
                  pidTokenMatches, controlAuthenticated, networkAssigned,
                  blockAssigned, resetFailed, recoveryRejected,
                  launchInputsTrusted, launchRejected, pidfdIdentityBound,
                  hostDisplayActive, displaySafetyPassed, resetScopeSafe,
                  resetSafetyPassed, dmaBindSafe, dmaSafetyPassed,
                  vbiosSupplySafe, vbiosSupplyPassed>>

ObserveSuccessfulChildExit ==
    /\ ObserveExactChildExitBase
    /\ childExitFailed' = FALSE
    /\ supervisorAcceptedExit' = TRUE
    /\ forcedStopUsed' = FALSE

ObserveFailedChildExit ==
    /\ ObserveExactChildExitBase
    /\ childExitFailed' = TRUE
    /\ supervisorAcceptedExit' = FALSE
    /\ forcedStopUsed' \in BOOLEAN

AcquireRecoveryPidfd ==
    /\ phase \in {Starting, Ready, Stopping}
    /\ runtimeRecord
    /\ childAlive
    /\ pidTokenMatches
    /\ ~pidfdIdentityBound
    /\ pidfdIdentityBound' = TRUE
    /\ qmpPowerdownAttempted' = TRUE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected>>

ObserveRecoveredChildExit ==
    /\ phase \in {Starting, Ready, Stopping}
    /\ childAlive
    /\ pidTokenMatches
    /\ pidfdIdentityBound
    /\ phase' = Stopped
    /\ childAlive' = FALSE
    /\ iommuFd' = FALSE
    /\ nonIdentityMap' = FALSE
    /\ pidfdIdentityBound' = FALSE
    /\ qemuExitObserved' = TRUE
    /\ forcedStopUsed' \in BOOLEAN
    /\ childExitFailed' = forcedStopUsed'
    /\ supervisorAcceptedExit' = FALSE
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, runtimeRecord,
                  pidTokenMatches, controlAuthenticated, networkAssigned,
                  blockAssigned, resetFailed, recoveryRejected,
                  launchInputsTrusted, launchRejected>>

SkipSignalForReusedNumericPid ==
    /\ phase \in {Starting, Ready, Stopping}
    /\ runtimeRecord
    /\ childAlive
    /\ pidTokenMatches
    /\ ~pidfdIdentityBound
    /\ phase' = Stopped
    /\ childAlive' = FALSE
    /\ pidTokenMatches' = FALSE
    /\ iommuFd' = FALSE
    /\ nonIdentityMap' = FALSE
    /\ controlAuthenticated' = FALSE
    /\ recoveryRejected' = TRUE
    /\ qemuExitObserved' = TRUE
    /\ forcedStopUsed' = FALSE
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, runtimeRecord,
                  networkAssigned, blockAssigned, resetFailed,
                  launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

ResetAfterStop ==
    /\ phase = Stopped
    /\ ~childAlive
    /\ qemuExitObserved
    /\ vfioBound
    /\ postStopReset' = TRUE
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, iommuFd, nonIdentityMap, runtimeRecord,
                  childAlive, pidTokenMatches, controlAuthenticated,
                  networkAssigned, blockAssigned, resetFailed,
                  recoveryRejected, launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

RestoreOriginalDrivers ==
    /\ phase = Stopped
    /\ ~childAlive
    /\ qemuExitObserved
    /\ postStopReset
    /\ phase' = Restored
    /\ durableLease' = FALSE
    /\ vfioBound' = FALSE
    /\ originalDriverBound' = TRUE
    /\ runtimeRecord' = FALSE
    /\ pidTokenMatches' = FALSE
    /\ pidfdIdentityBound' = FALSE
    /\ UNCHANGED <<preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  childAlive, controlAuthenticated, networkAssigned,
                  blockAssigned, resetFailed, recoveryRejected,
                  launchInputsTrusted, launchRejected>>

FailResetAndQuarantine ==
    /\ phase \in {Prepared, Stopped}
    /\ vfioBound
    /\ ~childAlive
    /\ phase' = Quarantined
    /\ resetFailed' = TRUE
    /\ originalDriverBound' = FALSE
    /\ durableLease' = TRUE
    /\ UNCHANGED <<vfioBound, preLaunchReset, postStopReset, iommuFd,
                  nonIdentityMap, runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  recoveryRejected, launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

IdleStep == UNCHANGED vars

Next ==
    \/ (VerifyTrustedLaunchInputs /\ UNCHANGED stableAdmissionVars)
    \/ (RejectMutableLaunchInputs /\ UNCHANGED stableAdmissionVars)
    \/ (VerifyPhysicalRuntimePreflight /\ UNCHANGED outcomeVars)
    \/ (HostDisplayBecomesActiveBeforeBind /\
         UNCHANGED <<outcomeVars, runtimePreflightPassed>>)
    \/ (RejectActiveHostDisplay /\ UNCHANGED outcomeVars)
    \/ (RejectUnsafeResetScope /\ UNCHANGED outcomeVars)
    \/ (RejectUnsafeDmaBind /\ UNCHANGED outcomeVars)
    \/ (RejectUnsafeVbiosSupply /\ UNCHANGED outcomeVars)
    \/ (ResetScopeBecomesUnsafeBeforeBind /\ UNCHANGED outcomeVars)
    \/ (DmaBindBecomesUnsafeBeforeBind /\ UNCHANGED outcomeVars)
    \/ (VbiosSupplyBecomesUnsafeBeforeBind /\ UNCHANGED outcomeVars)
    \/ (PrepareAuthorizedGroup /\ UNCHANGED stableAdmissionVars)
    \/ (ResetBeforeLaunch /\ UNCHANGED stableAdmissionVars)
    \/ (LaunchWithIommuFd /\ UNCHANGED stableAdmissionVars)
    \/ (AuthenticateControlAndDisplay /\ UNCHANGED stableAdmissionVars)
    \/ (RequestStop /\
         UNCHANGED <<childExitFailed, supervisorAcceptedExit,
                     qemuExitObserved, forcedStopUsed, admissionVars>>)
    \/ (LoseAuthenticatedHealth /\
         UNCHANGED <<childExitFailed, supervisorAcceptedExit,
                     qemuExitObserved, forcedStopUsed, admissionVars>>)
    \/ (ObserveSuccessfulChildExit /\
         UNCHANGED <<qmpPowerdownAttempted, admissionVars>>)
    \/ (ObserveFailedChildExit /\
         UNCHANGED <<qmpPowerdownAttempted, admissionVars>>)
    \/ (AcquireRecoveryPidfd /\
         UNCHANGED <<childExitFailed, supervisorAcceptedExit,
                     qemuExitObserved, forcedStopUsed, admissionVars>>)
    \/ (ObserveRecoveredChildExit /\
         UNCHANGED <<qmpPowerdownAttempted, admissionVars>>)
    \/ (SkipSignalForReusedNumericPid /\
         UNCHANGED <<childExitFailed, supervisorAcceptedExit,
                     qmpPowerdownAttempted, admissionVars>>)
    \/ (ResetAfterStop /\ UNCHANGED stableAdmissionVars)
    \/ (RestoreOriginalDrivers /\ UNCHANGED stableAdmissionVars)
    \/ (FailResetAndQuarantine /\ UNCHANGED stableAdmissionVars)
    \/ IdleStep

TypeOK ==
    /\ phase \in {Idle, Prepared, Starting, Ready, Stopping, Stopped,
                   Restored, Quarantined}
    /\ durableLease \in BOOLEAN
    /\ vfioBound \in BOOLEAN
    /\ originalDriverBound \in BOOLEAN
    /\ preLaunchReset \in BOOLEAN
    /\ postStopReset \in BOOLEAN
    /\ iommuFd \in BOOLEAN
    /\ nonIdentityMap \in BOOLEAN
    /\ runtimeRecord \in BOOLEAN
    /\ childAlive \in BOOLEAN
    /\ pidTokenMatches \in BOOLEAN
    /\ controlAuthenticated \in BOOLEAN
    /\ networkAssigned \in BOOLEAN
    /\ blockAssigned \in BOOLEAN
    /\ resetFailed \in BOOLEAN
    /\ recoveryRejected \in BOOLEAN
    /\ launchInputsTrusted \in BOOLEAN
    /\ launchRejected \in BOOLEAN
    /\ pidfdIdentityBound \in BOOLEAN
    /\ runtimePreflightPassed \in BOOLEAN
    /\ hostDisplayActive \in BOOLEAN
    /\ displaySafetyPassed \in BOOLEAN
    /\ resetScopeSafe \in BOOLEAN
    /\ resetSafetyPassed \in BOOLEAN
    /\ dmaBindSafe \in BOOLEAN
    /\ dmaSafetyPassed \in BOOLEAN
    /\ vbiosSupplySafe \in BOOLEAN
    /\ vbiosSupplyPassed \in BOOLEAN
    /\ childExitFailed \in BOOLEAN
    /\ supervisorAcceptedExit \in BOOLEAN
    /\ qmpPowerdownAttempted \in BOOLEAN
    /\ qemuExitObserved \in BOOLEAN
    /\ forcedStopUsed \in BOOLEAN

ChildHasCompleteAuthorityChain ==
    childAlive /\ pidTokenMatches =>
        /\ durableLease
        /\ vfioBound
        /\ ~originalDriverBound
        /\ preLaunchReset
        /\ iommuFd
        /\ nonIdentityMap
        /\ runtimeRecord
        /\ launchInputsTrusted
        /\ vbiosSupplyPassed
        /\ vbiosSupplySafe

ReadyRequiresAuthenticatedControlAndDisplay ==
    phase = Ready => controlAuthenticated

OriginalAndVfioNeverCoexist == ~(originalDriverBound /\ vfioBound)

RestorationRequiresStoppedReset ==
    phase = Restored =>
        /\ ~childAlive
        /\ qemuExitObserved
        /\ postStopReset
        /\ originalDriverBound
        /\ ~vfioBound
        /\ ~durableLease
        /\ ~runtimeRecord

QuarantineRetainsFailClosedOwnership ==
    phase = Quarantined =>
        /\ resetFailed
        /\ durableLease
        /\ vfioBound
        /\ ~originalDriverBound
        /\ ~childAlive

RuntimeRecordRetainsLease == runtimeRecord => durableLease

VfioBindingRequiresPhysicalRuntimePreflight ==
    vfioBound => runtimePreflightPassed

ActiveHostDisplayIsNeverAssigned ==
    vfioBound => displaySafetyPassed /\ ~hostDisplayActive

DisplaySafetyEvidenceIsCurrent ==
    displaySafetyPassed => ~hostDisplayActive

VfioBindingRequiresIsolatedResetScope ==
    vfioBound => resetSafetyPassed /\ resetScopeSafe

ResetSafetyEvidenceIsCurrent ==
    resetSafetyPassed => resetScopeSafe

VfioBindingRequiresDmaSafeBind ==
    vfioBound => dmaSafetyPassed /\ dmaBindSafe

DmaSafetyEvidenceIsCurrent ==
    dmaSafetyPassed => dmaBindSafe

VfioBindingRequiresExactAmdVbios ==
    vfioBound => vbiosSupplyPassed /\ vbiosSupplySafe

VbiosSupplyEvidenceIsCurrent ==
    vbiosSupplyPassed => vbiosSupplySafe

UntrustedOrMutableInputsNeverLaunch ==
    launchRejected => phase = Idle /\ ~childAlive /\ ~durableLease

RecoverySignalUsesExactPidfd ==
    pidfdIdentityBound =>
        /\ childAlive
        /\ runtimeRecord
        /\ pidTokenMatches
        /\ phase \in {Starting, Ready, Stopping}

ReusedNumericPidIsNeverSignaled ==
    recoveryRejected =>
        /\ ~childAlive
        /\ ~pidTokenMatches
        /\ ~pidfdIdentityBound

FailedChildExitIsNeverAccepted ==
    childExitFailed => ~supervisorAcceptedExit

PowerdownRequestAloneIsNeverExitEvidence ==
    qmpPowerdownAttempted /\ ~qemuExitObserved => childAlive

SupervisorAcceptanceRequiresObservedCleanExit ==
    supervisorAcceptedExit =>
        /\ qemuExitObserved
        /\ ~childExitFailed
        /\ ~forcedStopUsed

ForcedStopIsNeverAccepted == forcedStopUsed => ~supervisorAcceptedExit

ExcludedPhysicalDevicesStayDisabled == ~networkAssigned /\ ~blockAssigned

Spec == Init /\ [][Next]_vars
=============================================================================
