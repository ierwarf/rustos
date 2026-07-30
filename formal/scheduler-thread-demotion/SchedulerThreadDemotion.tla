----------------------- MODULE SchedulerThreadDemotion -----------------------
EXTENDS Naturals

(*******************************************************************************
Models per-thread self-demotion after POSIX clone inherited a process's
System-class admission.

Concrete owners:
  * kernel/ps/src/multitask/scheduler.rs
    `demote_current_user_task_to_user_class`
  * kernel/compat/src/user/syscall/linux/scheduler_ops.rs
    `SYS_RUSTOS_SCHED_DEMOTE_SELF`
  * services/uiserver/src/{sys.rs,main.rs,input_loop.rs,runtime_sync.rs}
  * services/loaderd/src/main.rs
  * services/vfsd/src/main.rs

The ABI deliberately has one direction.  A helper may surrender its base
System class, but can never raise itself again.  The only remaining temporary
System class is a reply-scoped IPC donation; that donation does not rewrite
the base class and must disappear when its reply is released or the thread is
retired.  This keeps background/untrusted helper work from competing with the
input/present owner while preserving bounded priority inheritance for an
already-authorized synchronous request.

Loaderd and vfsd additionally carry the terminal UI bootstrap replies.  They
may publish a demotion intent while processing that request, but must not
consume it until the exact reply succeeds.  Otherwise a still-blocked System
caller can be stranded after the server has already dropped to User class.
*******************************************************************************)

CONSTANTS Threads, CompletionBoundThreads

System == "system"
User == "user"
NoReply == "none"
ReplyPending == "pending"
ReplyComplete == "complete"

VARIABLES live, baseClass, demoted, replyDonation, bootstrapReply

vars == <<live, baseClass, demoted, replyDonation, bootstrapReply>>

Init ==
    /\ CompletionBoundThreads \subseteq Threads
    /\ live = [thread \in Threads |-> TRUE]
    /\ baseClass = [thread \in Threads |-> System]
    /\ demoted = [thread \in Threads |-> FALSE]
    /\ replyDonation = [thread \in Threads |-> FALSE]
    /\ bootstrapReply =
        [thread \in Threads |->
            IF thread \in CompletionBoundThreads THEN ReplyPending ELSE NoReply]

CompleteBootstrapReply(thread) ==
    /\ thread \in CompletionBoundThreads
    /\ live[thread]
    /\ bootstrapReply[thread] = ReplyPending
    /\ bootstrapReply' = [bootstrapReply EXCEPT ![thread] = ReplyComplete]
    /\ UNCHANGED <<live, baseClass, demoted, replyDonation>>

DemoteSelf(thread) ==
    /\ thread \in Threads
    /\ live[thread]
    /\ baseClass[thread] = System
    /\ (thread \in CompletionBoundThreads => bootstrapReply[thread] = ReplyComplete)
    /\ baseClass' = [baseClass EXCEPT ![thread] = User]
    /\ demoted' = [demoted EXCEPT ![thread] = TRUE]
    /\ UNCHANGED <<live, replyDonation, bootstrapReply>>

GrantReplyDonation(thread) ==
    /\ thread \in Threads
    /\ live[thread]
    /\ replyDonation' = [replyDonation EXCEPT ![thread] = TRUE]
    /\ UNCHANGED <<live, baseClass, demoted, bootstrapReply>>

ReleaseReplyDonation(thread) ==
    /\ thread \in Threads
    /\ replyDonation[thread]
    /\ replyDonation' = [replyDonation EXCEPT ![thread] = FALSE]
    /\ UNCHANGED <<live, baseClass, demoted, bootstrapReply>>

Retire(thread) ==
    /\ thread \in Threads
    /\ live[thread]
    /\ live' = [live EXCEPT ![thread] = FALSE]
    /\ replyDonation' = [replyDonation EXCEPT ![thread] = FALSE]
    /\ UNCHANGED <<baseClass, demoted, bootstrapReply>>

Next ==
    \/ \E thread \in Threads : CompleteBootstrapReply(thread)
    \/ \E thread \in Threads : DemoteSelf(thread)
    \/ \E thread \in Threads : GrantReplyDonation(thread)
    \/ \E thread \in Threads : ReleaseReplyDonation(thread)
    \/ \E thread \in Threads : Retire(thread)

EffectiveClass(thread) ==
    IF baseClass[thread] = System \/ replyDonation[thread]
    THEN System ELSE User

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ live \in [Threads -> BOOLEAN]
    /\ baseClass \in [Threads -> {System, User}]
    /\ demoted \in [Threads -> BOOLEAN]
    /\ replyDonation \in [Threads -> BOOLEAN]
    /\ bootstrapReply \in [Threads -> {NoReply, ReplyPending, ReplyComplete}]

NoSelfPromotion ==
    \A thread \in Threads : demoted[thread] => baseClass[thread] = User

DemotionIsBaseOnly ==
    \A thread \in Threads :
        demoted[thread] /\ replyDonation[thread] => EffectiveClass(thread) = System

EffectiveSystemNeedsAuthority ==
    \A thread \in Threads :
        EffectiveClass(thread) = System =>
            baseClass[thread] = System \/ replyDonation[thread]

RetiredThreadHasNoDonation ==
    \A thread \in Threads : \neg live[thread] => \neg replyDonation[thread]

NoDemotionBeforeBootstrapReply ==
    \A thread \in CompletionBoundThreads :
        demoted[thread] => bootstrapReply[thread] = ReplyComplete

=============================================================================
