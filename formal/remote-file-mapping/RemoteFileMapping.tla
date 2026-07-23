-------------------------- MODULE RemoteFileMapping --------------------------
EXTENDS Naturals

(*******************************************************************************
Composes loaderd's prepared file mapping with kernel-compat copy admission,
vfsd source ownership, the versioned VFS inline reply, and the immutable
early-system broker.

The early-system owner is resolved before its smaller transfer bound is
applied. Once that immutable owner is selected, disappearance is corruption
and cannot fall through to the mutable DVM volume. Every source is chunked to
its own wire capacity, and a short or failed read aborts instead of committing
a zero-filled executable tail.
*******************************************************************************)

CONSTANTS MaxBytes, VfsChunk, EarlyChunk

Sources == {"unknown", "early-system", "dvm-volume"}
Phases == {"inspect", "ready", "reading", "complete", "committed", "aborted"}

VARIABLES phase, source, expected, copied, requested, ownerResolved,
          shortReadRejected, fallbackAfterOwnership, zeroFilled

vars == <<phase, source, expected, copied, requested, ownerResolved,
          shortReadRejected, fallbackAfterOwnership, zeroFilled>>

Min(left, right) == IF left <= right THEN left ELSE right

Init ==
    /\ phase = "inspect"
    /\ source = "unknown"
    /\ expected \in 1..MaxBytes
    /\ copied = 0
    /\ requested = 0
    /\ ownerResolved = FALSE
    /\ shortReadRejected = FALSE
    /\ fallbackAfterOwnership = FALSE
    /\ zeroFilled = FALSE

ResolveEarlySystem ==
    /\ phase = "inspect"
    /\ phase' = "ready"
    /\ source' = "early-system"
    /\ ownerResolved' = TRUE
    /\ UNCHANGED <<expected, copied, requested, shortReadRejected,
                   fallbackAfterOwnership, zeroFilled>>

ResolveDvmVolume ==
    /\ phase = "inspect"
    /\ phase' = "ready"
    /\ source' = "dvm-volume"
    /\ UNCHANGED <<expected, copied, requested, ownerResolved,
                   shortReadRejected, fallbackAfterOwnership, zeroFilled>>

RequestChunk ==
    /\ phase = "ready"
    /\ copied < expected
    /\ phase' = "reading"
    /\ requested' =
        IF source = "early-system"
        THEN Min(EarlyChunk, expected - copied)
        ELSE Min(VfsChunk, expected - copied)
    /\ UNCHANGED <<source, expected, copied, ownerResolved,
                   shortReadRejected, fallbackAfterOwnership, zeroFilled>>

DeliverExact ==
    /\ phase = "reading"
    /\ requested > 0
    /\ copied' = copied + requested
    /\ requested' = 0
    /\ phase' = IF copied' = expected THEN "complete" ELSE "ready"
    /\ UNCHANGED <<source, expected, ownerResolved, shortReadRejected,
                   fallbackAfterOwnership, zeroFilled>>

RejectShortRead ==
    /\ phase = "reading"
    /\ requested > 0
    /\ phase' = "aborted"
    /\ requested' = 0
    /\ shortReadRejected' = TRUE
    /\ UNCHANGED <<source, expected, copied, ownerResolved,
                   fallbackAfterOwnership, zeroFilled>>

RejectOwnedDisappearance ==
    /\ phase = "reading"
    /\ source = "early-system"
    /\ ownerResolved
    /\ phase' = "aborted"
    /\ requested' = 0
    /\ UNCHANGED <<source, expected, copied, ownerResolved,
                   shortReadRejected, fallbackAfterOwnership, zeroFilled>>

RejectTransportFailure ==
    /\ phase = "reading"
    /\ source = "dvm-volume"
    /\ phase' = "aborted"
    /\ requested' = 0
    /\ UNCHANGED <<source, expected, copied, ownerResolved,
                   shortReadRejected, fallbackAfterOwnership, zeroFilled>>

Commit ==
    /\ phase = "complete"
    /\ copied = expected
    /\ phase' = "committed"
    /\ UNCHANGED <<source, expected, copied, requested, ownerResolved,
                   shortReadRejected, fallbackAfterOwnership, zeroFilled>>

Next ==
    ResolveEarlySystem
    \/ ResolveDvmVolume
    \/ RequestChunk
    \/ DeliverExact
    \/ RejectShortRead
    \/ RejectOwnedDisappearance
    \/ RejectTransportFailure
    \/ Commit

TypeOK ==
    /\ phase \in Phases
    /\ source \in Sources
    /\ expected \in 1..MaxBytes
    /\ copied \in 0..MaxBytes
    /\ requested \in 0..MaxBytes
    /\ ownerResolved \in BOOLEAN
    /\ shortReadRejected \in BOOLEAN
    /\ fallbackAfterOwnership \in BOOLEAN
    /\ zeroFilled \in BOOLEAN

CopiedNeverExceedsMapping == copied <= expected

ChunkRespectsSelectedWire ==
    phase = "reading" =>
        /\ requested > 0
        /\ requested <= expected - copied
        /\ IF source = "early-system"
           THEN requested <= EarlyChunk
           ELSE requested <= VfsChunk

CommitRequiresExactBytes ==
    phase = "committed" => copied = expected

ImmutableOwnershipNeverFallsBack ==
    ownerResolved => source = "early-system" /\ ~fallbackAfterOwnership

ShortReadNeverZeroFills == shortReadRejected => ~zeroFilled

TerminalHasNoPendingRequest ==
    phase \in {"committed", "aborted"} => requested = 0

Spec == Init /\ [][Next]_vars
===============================================================================
