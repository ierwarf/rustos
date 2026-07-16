---------------------- MODULE DvmCommercialLifecycle -----------------------
EXTENDS Naturals

(*******************************************************************************
Commercial physical-device DVM lifecycle contract.

Concrete owners:
  tools/hostd/src/runtime.rs
  libs/driver-domain-host/src/lib.rs

This model begins after signed release authorization has admitted one complete
IOMMU group.  Binding first requires the reversible physical-runtime preflight:
trusted exact QEMU/policy/artifacts, a usable IOMMUFD, and live sysfs evidence
that the group is neither the L0 boot display nor driving a connected DRM
connector.  Launch then requires
a durable active lease, every group member bound
to VFIO, a successful pre-launch reset, an IOMMUFD-backed non-identity address
space, and a private runtime record.  Readiness additionally requires an
authenticated control exchange whose inventory proves a supported DRM driver
and the live direct-scanout relay lock.  Normal restoration is legal only after the
exact recorded process has stopped and the whole group has reset again.  Any
reset failure retains VFIO ownership and a durable quarantine record.
An unsuccessful spontaneous or requested child exit may still be reset and
restored safely, but the supervisor must never accept it as a successful run.

Signed inputs are admitted only after their canonical files, owning directory
chain, executable, and detached-signature verifier are non-mutable by another
uid.  Crash recovery binds the checked PID/start token to a pidfd before it can
signal; a reused numeric PID is rejected and never receives a signal.

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
          childExitFailed,
          supervisorAcceptedExit

vars == <<phase, durableLease, vfioBound, originalDriverBound,
          preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
          runtimeRecord, childAlive, pidTokenMatches, controlAuthenticated,
          networkAssigned, blockAssigned, resetFailed, recoveryRejected,
          launchInputsTrusted, launchRejected, pidfdIdentityBound,
          runtimePreflightPassed, hostDisplayActive, displaySafetyPassed,
          childExitFailed, supervisorAcceptedExit>>

outcomeVars == <<childExitFailed, supervisorAcceptedExit>>
stableAdmissionVars == <<outcomeVars, runtimePreflightPassed,
                         hostDisplayActive, displaySafetyPassed>>

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
    /\ childExitFailed = FALSE
    /\ supervisorAcceptedExit = FALSE

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
    /\ runtimePreflightPassed' = TRUE
    /\ displaySafetyPassed' = TRUE
    /\ UNCHANGED hostDisplayActive
    /\ UNCHANGED <<phase, durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, iommuFd, nonIdentityMap,
                  runtimeRecord, childAlive, pidTokenMatches,
                  controlAuthenticated, networkAssigned, blockAssigned,
                  resetFailed, recoveryRejected, launchInputsTrusted,
                  launchRejected, pidfdIdentityBound>>

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
                  pidfdIdentityBound, hostDisplayActive>>

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
                  launchRejected, pidfdIdentityBound>>

PrepareAuthorizedGroup ==
    /\ phase = Idle
    /\ launchInputsTrusted
    /\ runtimePreflightPassed
    /\ displaySafetyPassed
    /\ ~hostDisplayActive
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
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, runtimeRecord,
                  pidTokenMatches, controlAuthenticated, networkAssigned,
                  blockAssigned, resetFailed, recoveryRejected,
                  launchInputsTrusted, launchRejected, pidfdIdentityBound,
                  hostDisplayActive, displaySafetyPassed>>

ObserveSuccessfulChildExit ==
    /\ ObserveExactChildExitBase
    /\ childExitFailed' = FALSE
    /\ supervisorAcceptedExit' = TRUE

ObserveFailedChildExit ==
    /\ ObserveExactChildExitBase
    /\ childExitFailed' = TRUE
    /\ supervisorAcceptedExit' = FALSE

AcquireRecoveryPidfd ==
    /\ phase \in {Starting, Ready, Stopping}
    /\ runtimeRecord
    /\ childAlive
    /\ pidTokenMatches
    /\ ~pidfdIdentityBound
    /\ pidfdIdentityBound' = TRUE
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
    /\ UNCHANGED <<durableLease, vfioBound, originalDriverBound,
                  preLaunchReset, postStopReset, runtimeRecord,
                  networkAssigned, blockAssigned, resetFailed,
                  launchInputsTrusted, launchRejected,
                  pidfdIdentityBound>>

ResetAfterStop ==
    /\ phase = Stopped
    /\ ~childAlive
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
    \/ (PrepareAuthorizedGroup /\ UNCHANGED stableAdmissionVars)
    \/ (ResetBeforeLaunch /\ UNCHANGED stableAdmissionVars)
    \/ (LaunchWithIommuFd /\ UNCHANGED stableAdmissionVars)
    \/ (AuthenticateControlAndDisplay /\ UNCHANGED stableAdmissionVars)
    \/ (RequestStop /\ UNCHANGED stableAdmissionVars)
    \/ (LoseAuthenticatedHealth /\ UNCHANGED stableAdmissionVars)
    \/ (ObserveSuccessfulChildExit /\ UNCHANGED runtimePreflightPassed)
    \/ (ObserveFailedChildExit /\ UNCHANGED runtimePreflightPassed)
    \/ (AcquireRecoveryPidfd /\ UNCHANGED stableAdmissionVars)
    \/ (ObserveRecoveredChildExit /\ UNCHANGED stableAdmissionVars)
    \/ (SkipSignalForReusedNumericPid /\ UNCHANGED stableAdmissionVars)
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
    /\ childExitFailed \in BOOLEAN
    /\ supervisorAcceptedExit \in BOOLEAN

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

ReadyRequiresAuthenticatedControlAndDisplay ==
    phase = Ready => controlAuthenticated

OriginalAndVfioNeverCoexist == ~(originalDriverBound /\ vfioBound)

RestorationRequiresStoppedReset ==
    phase = Restored =>
        /\ ~childAlive
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

ExcludedPhysicalDevicesStayDisabled == ~networkAssigned /\ ~blockAssigned

Spec == Init /\ [][Next]_vars
=============================================================================
