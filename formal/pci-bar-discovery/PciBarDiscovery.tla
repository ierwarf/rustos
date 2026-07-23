-------------------------- MODULE PciBarDiscovery ----------------------------
EXTENDS Naturals

(*******************************************************************************
Composes one standard PCI BAR discovery transaction across command decoding,
the low/high configuration dwords, size-mask decoding, and publication.

For a 64-bit BAR the low dword is restored before the high dword is probed.
No resource may be decoded or published from a transient all-ones BAR value,
and the original command and both BAR halves are restored on every terminal.
The size decoder uses the least significant implemented mask bit, so devices
that implement fewer than 64 address bits do not become enormous resources.
*******************************************************************************)

Phases == {
    "enabled", "quiesced", "low-probed", "low-restored", "high-probed",
    "bars-restored", "decoded", "published", "rejected"
}

VARIABLES phase, commandEnabled, lowRestored, highRestored, sizeValid, is64

vars == <<phase, commandEnabled, lowRestored, highRestored, sizeValid, is64>>

Init ==
    /\ phase = "enabled"
    /\ commandEnabled = TRUE
    /\ lowRestored = TRUE
    /\ highRestored = TRUE
    /\ sizeValid \in BOOLEAN
    /\ is64 \in BOOLEAN

DisableDecode ==
    /\ phase = "enabled"
    /\ phase' = "quiesced"
    /\ commandEnabled' = FALSE
    /\ UNCHANGED <<lowRestored, highRestored, sizeValid, is64>>

ProbeLow ==
    /\ phase = "quiesced"
    /\ ~commandEnabled
    /\ phase' = "low-probed"
    /\ lowRestored' = FALSE
    /\ UNCHANGED <<commandEnabled, highRestored, sizeValid, is64>>

RestoreLow ==
    /\ phase = "low-probed"
    /\ phase' = "low-restored"
    /\ lowRestored' = TRUE
    /\ UNCHANGED <<commandEnabled, highRestored, sizeValid, is64>>

ProbeHigh ==
    /\ phase = "low-restored"
    /\ is64
    /\ lowRestored
    /\ phase' = "high-probed"
    /\ highRestored' = FALSE
    /\ UNCHANGED <<commandEnabled, lowRestored, sizeValid, is64>>

RestoreHigh ==
    /\ phase = "high-probed"
    /\ phase' = "bars-restored"
    /\ highRestored' = TRUE
    /\ UNCHANGED <<commandEnabled, lowRestored, sizeValid, is64>>

SkipHigh ==
    /\ phase = "low-restored"
    /\ ~is64
    /\ phase' = "bars-restored"
    /\ UNCHANGED <<commandEnabled, lowRestored, highRestored, sizeValid, is64>>

Decode ==
    /\ phase = "bars-restored"
    /\ lowRestored
    /\ highRestored
    /\ sizeValid
    /\ phase' = "decoded"
    /\ UNCHANGED <<commandEnabled, lowRestored, highRestored, sizeValid, is64>>

Publish ==
    /\ phase = "decoded"
    /\ phase' = "published"
    /\ commandEnabled' = TRUE
    /\ UNCHANGED <<lowRestored, highRestored, sizeValid, is64>>

Reject ==
    /\ phase = "bars-restored"
    /\ ~sizeValid
    /\ phase' = "rejected"
    /\ commandEnabled' = TRUE
    /\ UNCHANGED <<lowRestored, highRestored, sizeValid, is64>>

Next ==
    DisableDecode
    \/ ProbeLow
    \/ RestoreLow
    \/ ProbeHigh
    \/ RestoreHigh
    \/ SkipHigh
    \/ Decode
    \/ Publish
    \/ Reject

TypeOK ==
    /\ phase \in Phases
    /\ commandEnabled \in BOOLEAN
    /\ lowRestored \in BOOLEAN
    /\ highRestored \in BOOLEAN
    /\ sizeValid \in BOOLEAN
    /\ is64 \in BOOLEAN

ProbeRequiresDecodeDisabled ==
    phase \in {"low-probed", "low-restored", "high-probed", "bars-restored",
               "decoded"} => ~commandEnabled

HighProbeRequiresRestoredLow ==
    phase = "high-probed" => lowRestored

DecodeRequiresRestoredPair ==
    phase \in {"decoded", "published"} => lowRestored /\ highRestored

PublishedResourceIsValid ==
    phase = "published" => sizeValid

EveryTerminalRestoresHardware ==
    phase \in {"published", "rejected"} =>
        commandEnabled /\ lowRestored /\ highRestored

Spec == Init /\ [][Next]_vars
===============================================================================
