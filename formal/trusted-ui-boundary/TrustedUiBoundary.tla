------------------------------- MODULE TrustedUiBoundary -----------------------
EXTENDS Naturals

(*******************************************************************************
Models the authority boundary for a trusted-attention / privileged prompt.

Concrete source anchors:
  * DVM scanout provenance: kernel/io-manager/src/io/dvm_display.rs and
    kernel/io-manager/src/io/gui.rs
  * ABI status: libs/rustos-user-abi/src/device.rs and syscall.rs
  * userspace policy answer: services/uiserver/src/sys.rs

The DVM display relay owns physical KMS and the DVM input relay only proves a
launch-bound agent/device path. Both are safe bounded transports, but neither
attests a human-visible display nor human intent. Consequently present RustOS
code returns unattested scanout and input blockers for every provider.

The model also contains future independently attested providers to prove the
admission rule itself: a prompt can be granted only while both such providers
remain live. A DVM may lose or corrupt its own channel but may never change its
provenance to Trusted or forge an independent attestation.
*******************************************************************************)

None == "none"
Dvm == "dvm"
Trusted == "trusted"

Idle == "idle"
Denied == "denied"
Granted == "granted"
Revoked == "revoked"

VARIABLES displaySource,
          inputSource,
          displayAttested,
          inputAttested,
          displayCompromised,
          inputCompromised,
          promptState

vars == <<displaySource, inputSource, displayAttested, inputAttested,
          displayCompromised, inputCompromised, promptState>>

TrustedUiReady ==
    /\ displaySource = Trusted
    /\ inputSource = Trusted
    /\ displayAttested
    /\ inputAttested
    /\ ~displayCompromised
    /\ ~inputCompromised

Init ==
    /\ displaySource = None
    /\ inputSource = None
    /\ displayAttested = FALSE
    /\ inputAttested = FALSE
    /\ displayCompromised = FALSE
    /\ inputCompromised = FALSE
    /\ promptState = Idle

InstallDvmDisplay ==
    /\ displaySource = None
    /\ displaySource' = Dvm
    /\ displayAttested' = FALSE
    /\ UNCHANGED <<inputSource, inputAttested, displayCompromised,
                  inputCompromised, promptState>>

InstallDvmInput ==
    /\ inputSource = None
    /\ inputSource' = Dvm
    /\ inputAttested' = FALSE
    /\ UNCHANGED <<displaySource, displayAttested, displayCompromised,
                  inputCompromised, promptState>>

InstallTrustedDisplay ==
    /\ displaySource = None
    /\ displaySource' = Trusted
    /\ displayAttested' = TRUE
    /\ UNCHANGED <<inputSource, inputAttested, displayCompromised,
                  inputCompromised, promptState>>

InstallTrustedInput ==
    /\ inputSource = None
    /\ inputSource' = Trusted
    /\ inputAttested' = TRUE
    /\ UNCHANGED <<displaySource, displayAttested, displayCompromised,
                  inputCompromised, promptState>>

DvmCompromiseDisplay ==
    /\ displaySource = Dvm
    /\ ~displayCompromised
    /\ displayCompromised' = TRUE
    /\ displayAttested' = FALSE
    /\ UNCHANGED <<displaySource, inputSource, inputAttested,
                  inputCompromised, promptState>>

DvmCompromiseInput ==
    /\ inputSource = Dvm
    /\ ~inputCompromised
    /\ inputCompromised' = TRUE
    /\ inputAttested' = FALSE
    /\ UNCHANGED <<displaySource, inputSource, displayAttested,
                  displayCompromised, promptState>>

\* An independently attested provider can remain physically present while its
\* attestation lease is withdrawn.  Treat that as immediate prompt revocation;
\* waiting for a device-removal event would leave a privileged decision on a
\* stale trust assertion.
RevokeDisplayAttestation ==
    /\ displaySource = Trusted
    /\ displayAttested
    /\ displayAttested' = FALSE
    /\ promptState' = IF promptState = Granted THEN Revoked ELSE promptState
    /\ UNCHANGED <<displaySource, inputSource, inputAttested,
                  displayCompromised, inputCompromised>>

RevokeInputAttestation ==
    /\ inputSource = Trusted
    /\ inputAttested
    /\ inputAttested' = FALSE
    /\ promptState' = IF promptState = Granted THEN Revoked ELSE promptState
    /\ UNCHANGED <<displaySource, inputSource, displayAttested,
                  displayCompromised, inputCompromised>>

LoseDisplay ==
    /\ displaySource # None
    /\ displaySource' = None
    /\ displayAttested' = FALSE
    /\ displayCompromised' = FALSE
    /\ promptState' = IF promptState = Granted THEN Revoked ELSE promptState
    /\ UNCHANGED <<inputSource, inputAttested, inputCompromised>>

LoseInput ==
    /\ inputSource # None
    /\ inputSource' = None
    /\ inputAttested' = FALSE
    /\ inputCompromised' = FALSE
    /\ promptState' = IF promptState = Granted THEN Revoked ELSE promptState
    /\ UNCHANGED <<displaySource, displayAttested, displayCompromised>>

RequestPrivilegedPrompt ==
    /\ promptState = Idle
    /\ promptState' = IF TrustedUiReady THEN Granted ELSE Denied
    /\ UNCHANGED <<displaySource, inputSource, displayAttested, inputAttested,
                  displayCompromised, inputCompromised>>

ResetPrompt ==
    /\ promptState \in {Denied, Revoked}
    /\ promptState' = Idle
    /\ UNCHANGED <<displaySource, inputSource, displayAttested, inputAttested,
                  displayCompromised, inputCompromised>>

CompletePrompt ==
    /\ promptState = Granted
    /\ TrustedUiReady
    /\ promptState' = Idle
    /\ UNCHANGED <<displaySource, inputSource, displayAttested, inputAttested,
                  displayCompromised, inputCompromised>>

Next ==
    \/ InstallDvmDisplay
    \/ InstallDvmInput
    \/ InstallTrustedDisplay
    \/ InstallTrustedInput
    \/ DvmCompromiseDisplay
    \/ DvmCompromiseInput
    \/ RevokeDisplayAttestation
    \/ RevokeInputAttestation
    \/ LoseDisplay
    \/ LoseInput
    \/ RequestPrivilegedPrompt
    \/ ResetPrompt
    \/ CompletePrompt

TypeOK ==
    /\ displaySource \in {None, Dvm, Trusted}
    /\ inputSource \in {None, Dvm, Trusted}
    /\ displayAttested \in BOOLEAN
    /\ inputAttested \in BOOLEAN
    /\ displayCompromised \in BOOLEAN
    /\ inputCompromised \in BOOLEAN
    /\ promptState \in {Idle, Denied, Granted, Revoked}

OnlyTrustedChannelsMayBeAttested ==
    /\ displayAttested => displaySource = Trusted
    /\ inputAttested => inputSource = Trusted

PrivilegedPromptRequiresIndependentTrustedUi ==
    promptState = Granted => TrustedUiReady

DvmTransportCannotAuthorizePrivilegedPrompt ==
    (displaySource = Dvm \/ inputSource = Dvm) => promptState # Granted

CompromiseCannotRetainTrustedPrompt ==
    (displayCompromised \/ inputCompromised) => promptState # Granted

AttestationRevocationCannotRetainTrustedPrompt ==
    (~displayAttested \/ ~inputAttested) => promptState # Granted

=============================================================================
