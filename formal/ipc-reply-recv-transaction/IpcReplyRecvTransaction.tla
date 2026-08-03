----------------------- MODULE IpcReplyRecvTransaction -----------------------
EXTENDS Integers

(*******************************************************************************
Models the byte-only RustOS reply-receive syscall as one phase-explicit
transaction.  User-controlled shape, output ranges, endpoint ownership, and
response bytes are preflighted while the old reply capability is still live.
Only then may the kernel consume that one-shot reply, wake the exact caller,
and enter the existing endpoint check-arm-recheck receive protocol.

Concrete owners:
  * libs/rustos-user-abi/src/syscall/ipc_reply_recv.rs defines the wire and
    disjoint post-commit error range.
  * kernel/compat/src/user/syscall/linux/ipc_reply_recv.rs preflights, commits
    reply, retains the exact caller handoff, and reuses the blocking receive.
  * libs/rustos-svc-runtime/src/ipc.rs decodes the phase-aware result.
  * services/inputd/src/service_loop.rs retries only a pre-commit live cap,
    never retries a tagged post-commit cap, and explicitly replies to malformed
    dequeued calls.
*******************************************************************************)

Preflight == "preflight"
ReplyCommitted == "reply-committed"
ReceiveArmed == "receive-armed"
Blocked == "blocked"
Delivered == "delivered"
PreFailed == "pre-failed"
PostFailed == "post-failed"

NoResult == "none"
Success == "success"
PreError == "pre-error"
TaggedPostError == "tagged-post-error"

VARIABLES phase, replyLive, callerRunnable, waiterArmed, serverBlocked,
          requestPending, nextReplyOwned, resultKind

vars == <<phase, replyLive, callerRunnable, waiterArmed, serverBlocked,
          requestPending, nextReplyOwned, resultKind>>

Init ==
    /\ phase = Preflight
    /\ replyLive = TRUE
    /\ callerRunnable = FALSE
    /\ waiterArmed = FALSE
    /\ serverBlocked = FALSE
    /\ requestPending \in BOOLEAN
    /\ nextReplyOwned = FALSE
    /\ resultKind = NoResult

PreflightReject ==
    /\ phase = Preflight
    /\ phase' = PreFailed
    /\ resultKind' = PreError
    /\ UNCHANGED <<replyLive, callerRunnable, waiterArmed, serverBlocked,
                    requestPending, nextReplyOwned>>

CommitReply ==
    /\ phase = Preflight
    /\ phase' = ReplyCommitted
    /\ replyLive' = FALSE
    /\ callerRunnable' = TRUE
    /\ UNCHANGED <<waiterArmed, serverBlocked, requestPending,
                    nextReplyOwned, resultKind>>

ReceiveQueued ==
    /\ phase = ReplyCommitted
    /\ requestPending
    /\ phase' = Delivered
    /\ requestPending' = FALSE
    /\ nextReplyOwned' = TRUE
    /\ resultKind' = Success
    /\ UNCHANGED <<replyLive, callerRunnable, waiterArmed, serverBlocked>>

ArmReceive ==
    /\ phase = ReplyCommitted
    /\ ~requestPending
    /\ phase' = ReceiveArmed
    /\ waiterArmed' = TRUE
    /\ UNCHANGED <<replyLive, callerRunnable, serverBlocked, requestPending,
                    nextReplyOwned, resultKind>>

PublishDuringRecheck ==
    /\ phase = ReceiveArmed
    /\ ~requestPending
    /\ phase' = Delivered
    /\ waiterArmed' = FALSE
    /\ nextReplyOwned' = TRUE
    /\ resultKind' = Success
    /\ UNCHANGED <<replyLive, callerRunnable, serverBlocked, requestPending>>

CommitBlock ==
    /\ phase = ReceiveArmed
    /\ ~requestPending
    /\ phase' = Blocked
    /\ serverBlocked' = TRUE
    /\ UNCHANGED <<replyLive, callerRunnable, waiterArmed, requestPending,
                    nextReplyOwned, resultKind>>

PublishAndWake ==
    /\ phase = Blocked
    /\ serverBlocked
    /\ phase' = Delivered
    /\ waiterArmed' = FALSE
    /\ serverBlocked' = FALSE
    /\ nextReplyOwned' = TRUE
    /\ resultKind' = Success
    /\ UNCHANGED <<replyLive, callerRunnable, requestPending>>

PostCommitFail ==
    /\ phase \in {ReplyCommitted, ReceiveArmed}
    /\ phase' = PostFailed
    /\ waiterArmed' = FALSE
    /\ serverBlocked' = FALSE
    /\ resultKind' = TaggedPostError
    /\ UNCHANGED <<replyLive, callerRunnable, requestPending, nextReplyOwned>>

TerminalStutter ==
    /\ phase \in {Delivered, PreFailed, PostFailed}
    /\ UNCHANGED vars

Next ==
    \/ PreflightReject
    \/ CommitReply
    \/ ReceiveQueued
    \/ ArmReceive
    \/ PublishDuringRecheck
    \/ CommitBlock
    \/ PublishAndWake
    \/ PostCommitFail
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {Preflight, ReplyCommitted, ReceiveArmed, Blocked,
                    Delivered, PreFailed, PostFailed}
    /\ replyLive \in BOOLEAN
    /\ callerRunnable \in BOOLEAN
    /\ waiterArmed \in BOOLEAN
    /\ serverBlocked \in BOOLEAN
    /\ requestPending \in BOOLEAN
    /\ nextReplyOwned \in BOOLEAN
    /\ resultKind \in {NoResult, Success, PreError, TaggedPostError}

PreCommitFailurePreservesReply ==
    phase = PreFailed =>
        replyLive /\ ~callerRunnable /\ ~nextReplyOwned /\ resultKind = PreError

ReplyCommitPrecedesReceive ==
    nextReplyOwned => ~replyLive /\ callerRunnable

ConsumedReplyWakesCaller ==
    ~replyLive => callerRunnable

PostCommitFailureIsTagged ==
    phase = PostFailed => ~replyLive /\ resultKind = TaggedPostError

UntaggedErrorMeansNoCommit ==
    resultKind = PreError => replyLive

BlockedReceiveHasWakeCustody ==
    serverBlocked =>
        phase = Blocked /\ waiterArmed /\ ~requestPending /\ ~replyLive

DeliveredRequestHasExactReplyCustody ==
    phase = Delivered =>
        nextReplyOwned /\ ~waiterArmed /\ ~serverBlocked /\ resultKind = Success

=============================================================================
