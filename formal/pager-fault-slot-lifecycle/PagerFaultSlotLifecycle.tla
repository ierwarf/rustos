--------------------- MODULE PagerFaultSlotLifecycle ---------------------
EXTENDS Naturals

(*******************************************************************************
Owner: kernel-ps fixed pager-fault custody.
Linearization points: lock-free fault-slot reservation, fused transition to
BlockedOnPager, reply claim, then exactly one consume or cancellation.  The
model deliberately has no allocator, process-state lock, or service lookup in
the fault-entry transition.
*******************************************************************************)

CONSTANTS Free, Pending, Blocked, Dispatched, Claimed, Cancelled, None

VARIABLES state, generation, token, claimCount, tokenExact, endpointExact, requestExact,
          schedulerBlocked, allocatorTouched, processStateLockTouched,
          serviceLookupTouched, dispatcherOwns
vars == <<state, generation, token, claimCount, tokenExact, endpointExact, requestExact,
          schedulerBlocked, allocatorTouched, processStateLockTouched,
          serviceLookupTouched, dispatcherOwns>>

Init ==
    /\ state = Free
    /\ generation = 0
    /\ token = 0
    /\ claimCount = 0
    /\ tokenExact = TRUE
    /\ endpointExact = TRUE
    /\ requestExact = TRUE
    /\ schedulerBlocked = FALSE
    /\ allocatorTouched = FALSE
    /\ processStateLockTouched = FALSE
    /\ serviceLookupTouched = FALSE
    /\ dispatcherOwns = FALSE

Reserve ==
    /\ state = Free
    /\ generation < 2
    /\ state' = Pending
    /\ generation' = generation + 1
    /\ token' = generation + 1
    /\ claimCount' = 0
    /\ tokenExact' = TRUE
    /\ endpointExact' = TRUE
    /\ requestExact' = TRUE
    /\ schedulerBlocked' = FALSE
    /\ dispatcherOwns' = FALSE
    /\ UNCHANGED <<allocatorTouched, processStateLockTouched, serviceLookupTouched>>

RejectMalformed ==
    /\ state = Free
    /\ UNCHANGED vars

CommitBlocked ==
    /\ state = Pending
    /\ state' = Blocked
    /\ schedulerBlocked' = TRUE
    /\ UNCHANGED <<generation, token, claimCount, tokenExact, endpointExact, requestExact,
                    allocatorTouched, processStateLockTouched, serviceLookupTouched,
                    dispatcherOwns>>

DispatchToPager ==
    /\ state = Blocked
    /\ schedulerBlocked
    /\ state' = Dispatched
    /\ dispatcherOwns' = TRUE
    /\ UNCHANGED <<generation, token, claimCount, tokenExact, endpointExact, requestExact,
                    schedulerBlocked, allocatorTouched, processStateLockTouched,
                    serviceLookupTouched>>

RejectEarlyReply ==
    /\ state = Pending
    /\ UNCHANGED vars

ClaimReply(replyToken, endpointMatches, requestMatches) ==
    /\ state = Dispatched
    /\ dispatcherOwns
    /\ replyToken = token
    /\ endpointMatches
    /\ requestMatches
    /\ state' = Claimed
    /\ claimCount' = claimCount + 1
    /\ tokenExact' = (replyToken = token)
    /\ endpointExact' = endpointMatches
    /\ requestExact' = requestMatches
    /\ UNCHANGED <<generation, token, schedulerBlocked, allocatorTouched,
                    processStateLockTouched, serviceLookupTouched, dispatcherOwns>>

RejectStaleReply(replyToken) ==
    /\ replyToken # token \/ state # Dispatched
    /\ UNCHANGED vars

Consume ==
    /\ state = Claimed
    /\ state' = Free
    /\ token' = 0
    /\ schedulerBlocked' = FALSE
    /\ dispatcherOwns' = FALSE
    /\ UNCHANGED <<generation, claimCount, tokenExact, endpointExact, requestExact,
                    allocatorTouched, processStateLockTouched, serviceLookupTouched>>

Cancel ==
    /\ state \in {Pending, Blocked, Dispatched}
    /\ state' = Cancelled
    /\ schedulerBlocked' = FALSE
    /\ dispatcherOwns' = FALSE
    /\ UNCHANGED <<generation, token, claimCount, tokenExact, endpointExact, requestExact,
                    allocatorTouched, processStateLockTouched, serviceLookupTouched>>

RecycleCancelled ==
    /\ state = Cancelled
    /\ state' = Free
    /\ token' = 0
    /\ UNCHANGED <<generation, claimCount, tokenExact, endpointExact, requestExact,
                    schedulerBlocked, allocatorTouched, processStateLockTouched,
                    serviceLookupTouched, dispatcherOwns>>

ObserveTerminal ==
    /\ state = Free /\ generation = 2
    /\ UNCHANGED vars

Next ==
    \/ Reserve
    \/ RejectMalformed
    \/ CommitBlocked
    \/ DispatchToPager
    \/ RejectEarlyReply
    \/ \E replyToken \in 0..2, endpointMatches \in BOOLEAN, requestMatches \in BOOLEAN:
         ClaimReply(replyToken, endpointMatches, requestMatches)
    \/ \E replyToken \in 0..2: RejectStaleReply(replyToken)
    \/ Consume
    \/ Cancel
    \/ RecycleCancelled
    \/ ObserveTerminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {Free, Pending, Blocked, Dispatched, Claimed, Cancelled}
    /\ generation \in 0..2
    /\ token \in 0..2
    /\ claimCount \in 0..1
    /\ tokenExact \in BOOLEAN
    /\ endpointExact \in BOOLEAN
    /\ requestExact \in BOOLEAN
    /\ schedulerBlocked \in BOOLEAN
    /\ allocatorTouched \in BOOLEAN
    /\ processStateLockTouched \in BOOLEAN
    /\ serviceLookupTouched \in BOOLEAN
    /\ dispatcherOwns \in BOOLEAN

BlockedHasExactToken == (state = Blocked) => token # 0 /\ schedulerBlocked
DispatchedHasExactOwner == (state = Dispatched) => dispatcherOwns /\ schedulerBlocked
ReplyClaimRequiresBlocked == (state = Claimed) => schedulerBlocked /\ tokenExact /\ endpointExact /\ requestExact
ReplyClaimRequiresDispatch == (state = Claimed) => dispatcherOwns
OneShotReplyClaim == claimCount <= 1
FaultEntryIsNonBlocking == ~allocatorTouched /\ ~processStateLockTouched /\ ~serviceLookupTouched

=============================================================================
