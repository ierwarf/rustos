--------------------- MODULE PagerFaultSlotLifecycle ---------------------
EXTENDS Naturals

(*******************************************************************************
Owner: kernel-ps fixed pager-fault custody.
Linearization points: lock-free fault-slot reservation, fused transition to
BlockedOnPager, endpoint-exact worker dispatch, durable scheduling-context
donation binding, worker-bound reply claim, then exactly one consume or
cancellation. The model deliberately has no allocator, process-state lock,
service lookup, or generic IPC runtime in the fault-entry transition.
*******************************************************************************)

CONSTANTS Free, Pending, Blocked, Dispatched, Claimed, Cancelled, None

VARIABLES state, generation, token, claimCount, tokenExact, endpointExact,
          requestExact, workerExact, schedulerBlocked, handoffDonated,
          ledgerBound, donationReleased, allocatorTouched,
          processStateLockTouched, serviceLookupTouched, genericIpcTouched,
          dispatcherOwns
vars == <<state, generation, token, claimCount, tokenExact, endpointExact,
          requestExact, workerExact, schedulerBlocked, handoffDonated,
          ledgerBound, donationReleased, allocatorTouched,
          processStateLockTouched, serviceLookupTouched, genericIpcTouched,
          dispatcherOwns>>

Init ==
    /\ state = Free
    /\ generation = 0
    /\ token = 0
    /\ claimCount = 0
    /\ tokenExact = TRUE
    /\ endpointExact = TRUE
    /\ requestExact = TRUE
    /\ workerExact = TRUE
    /\ schedulerBlocked = FALSE
    /\ handoffDonated = FALSE
    /\ ledgerBound = FALSE
    /\ donationReleased = TRUE
    /\ allocatorTouched = FALSE
    /\ processStateLockTouched = FALSE
    /\ serviceLookupTouched = FALSE
    /\ genericIpcTouched = FALSE
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
    /\ workerExact' = TRUE
    /\ schedulerBlocked' = FALSE
    /\ handoffDonated' = FALSE
    /\ ledgerBound' = FALSE
    /\ donationReleased' = FALSE
    /\ dispatcherOwns' = FALSE
    /\ UNCHANGED <<allocatorTouched, processStateLockTouched,
                    serviceLookupTouched, genericIpcTouched>>

RejectMalformed ==
    /\ state = Free
    /\ UNCHANGED vars

CommitBlocked ==
    /\ state = Pending
    /\ state' = Blocked
    /\ schedulerBlocked' = TRUE
    /\ UNCHANGED <<generation, token, claimCount, tokenExact, endpointExact,
                    requestExact, workerExact, handoffDonated, ledgerBound,
                    donationReleased, allocatorTouched, processStateLockTouched,
                    serviceLookupTouched, genericIpcTouched, dispatcherOwns>>

DispatchToPager(endpointMatches, workerMatches) ==
    /\ state = Blocked
    /\ schedulerBlocked
    /\ endpointMatches
    /\ workerMatches
    /\ state' = Dispatched
    /\ endpointExact' = endpointMatches
    /\ workerExact' = workerMatches
    /\ handoffDonated' = TRUE
    /\ dispatcherOwns' = TRUE
    /\ UNCHANGED <<generation, token, claimCount, tokenExact, requestExact,
                    schedulerBlocked, ledgerBound, donationReleased,
                    allocatorTouched, processStateLockTouched,
                    serviceLookupTouched, genericIpcTouched>>

BindDonation ==
    /\ state = Dispatched
    /\ dispatcherOwns
    /\ workerExact
    /\ ~ledgerBound
    /\ ledgerBound' = TRUE
    /\ donationReleased' = FALSE
    /\ UNCHANGED <<state, generation, token, claimCount, tokenExact,
                    endpointExact, requestExact, workerExact, schedulerBlocked,
                    handoffDonated, allocatorTouched, processStateLockTouched,
                    serviceLookupTouched, genericIpcTouched, dispatcherOwns>>

RejectUndonatedDispatch ==
    /\ state = Dispatched
    /\ dispatcherOwns
    /\ ~ledgerBound
    /\ state' = Cancelled
    /\ schedulerBlocked' = FALSE
    /\ donationReleased' = TRUE
    /\ dispatcherOwns' = FALSE
    /\ UNCHANGED <<generation, token, claimCount, tokenExact, endpointExact,
                    requestExact, workerExact, handoffDonated, ledgerBound,
                    allocatorTouched, processStateLockTouched,
                    serviceLookupTouched, genericIpcTouched>>

RejectEarlyReply ==
    /\ state \in {Pending, Blocked}
    /\ UNCHANGED vars

ClaimReply(replyToken, workerMatches, requestMatches) ==
    /\ state = Dispatched
    /\ dispatcherOwns
    /\ ledgerBound
    /\ replyToken = token
    /\ workerMatches
    /\ requestMatches
    /\ state' = Claimed
    /\ claimCount' = claimCount + 1
    /\ tokenExact' = (replyToken = token)
    /\ workerExact' = workerMatches
    /\ requestExact' = requestMatches
    /\ UNCHANGED <<generation, token, endpointExact, schedulerBlocked,
                    handoffDonated, ledgerBound, donationReleased,
                    allocatorTouched, processStateLockTouched,
                    serviceLookupTouched, genericIpcTouched, dispatcherOwns>>

RejectStaleReply(replyToken) ==
    /\ replyToken # token \/ state # Dispatched
    /\ UNCHANGED vars

Consume ==
    /\ state = Claimed
    /\ ledgerBound
    /\ state' = Free
    /\ token' = 0
    /\ schedulerBlocked' = FALSE
    /\ ledgerBound' = FALSE
    /\ donationReleased' = TRUE
    /\ dispatcherOwns' = FALSE
    /\ UNCHANGED <<generation, claimCount, tokenExact, endpointExact,
                    requestExact, workerExact, handoffDonated, allocatorTouched,
                    processStateLockTouched, serviceLookupTouched,
                    genericIpcTouched>>

Cancel ==
    /\ state \in {Pending, Blocked, Dispatched}
    /\ state' = Cancelled
    /\ schedulerBlocked' = FALSE
    /\ ledgerBound' = FALSE
    /\ donationReleased' = TRUE
    /\ dispatcherOwns' = FALSE
    /\ UNCHANGED <<generation, token, claimCount, tokenExact, endpointExact,
                    requestExact, workerExact, handoffDonated, allocatorTouched,
                    processStateLockTouched, serviceLookupTouched,
                    genericIpcTouched>>

RecycleCancelled ==
    /\ state = Cancelled
    /\ state' = Free
    /\ token' = 0
    /\ UNCHANGED <<generation, claimCount, tokenExact, endpointExact,
                    requestExact, workerExact, schedulerBlocked, handoffDonated,
                    ledgerBound, donationReleased, allocatorTouched,
                    processStateLockTouched, serviceLookupTouched,
                    genericIpcTouched, dispatcherOwns>>

ObserveTerminal ==
    /\ state = Free /\ generation = 2
    /\ UNCHANGED vars

Next ==
    \/ Reserve
    \/ RejectMalformed
    \/ CommitBlocked
    \/ \E endpointMatches \in BOOLEAN, workerMatches \in BOOLEAN:
         DispatchToPager(endpointMatches, workerMatches)
    \/ BindDonation
    \/ RejectUndonatedDispatch
    \/ RejectEarlyReply
    \/ \E replyToken \in 0..2, workerMatches \in BOOLEAN,
          requestMatches \in BOOLEAN:
         ClaimReply(replyToken, workerMatches, requestMatches)
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
    /\ workerExact \in BOOLEAN
    /\ schedulerBlocked \in BOOLEAN
    /\ handoffDonated \in BOOLEAN
    /\ ledgerBound \in BOOLEAN
    /\ donationReleased \in BOOLEAN
    /\ allocatorTouched \in BOOLEAN
    /\ processStateLockTouched \in BOOLEAN
    /\ serviceLookupTouched \in BOOLEAN
    /\ genericIpcTouched \in BOOLEAN
    /\ dispatcherOwns \in BOOLEAN

BlockedHasExactToken == (state = Blocked) => token # 0 /\ schedulerBlocked
DispatchedHasExactOwner ==
    (state = Dispatched) =>
        dispatcherOwns /\ schedulerBlocked /\ endpointExact /\ workerExact /\ handoffDonated
ReplyClaimRequiresBlocked ==
    (state = Claimed) =>
        schedulerBlocked /\ tokenExact /\ endpointExact /\ workerExact /\ requestExact
ReplyClaimRequiresDispatch == (state = Claimed) => dispatcherOwns
ReplyClaimRequiresDonation == (state = Claimed) => ledgerBound /\ ~donationReleased
DonationBoundOnlyWhileOwned ==
    ledgerBound => state \in {Dispatched, Claimed} /\ dispatcherOwns
DonationReleasedBeforeWake ==
    (state \in {Free, Cancelled}) => ~ledgerBound /\ donationReleased
OneShotReplyClaim == claimCount <= 1
FaultEntryIsNonBlocking ==
    ~allocatorTouched /\ ~processStateLockTouched /\ ~serviceLookupTouched
FaultPathBypassesGenericIpc == ~genericIpcTouched

=============================================================================
