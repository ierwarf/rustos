-------------------------- MODULE WaitSetRegistry --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Finite registry model for vfsd-owned persistent epoll objects and interests.

The model deliberately covers only durable registry admission and checkpoint
restore atomicity.  It does not model reverse-index lookup cost, readiness
queries, or kernel waiter ownership; those remain in UserspaceWaitSet.  The
concrete owner is services/vfsd/src/{lib.rs,state_checkpoint.rs}.

A restore request is one new object/interest batch.  Its admission checks run
before the live registry is changed: an invalid batch records a rejection and
leaves the exact prior registry visible, while an admitted batch adds exactly
its new objects and interests without replacing pre-existing registry state.
*******************************************************************************)

CONSTANTS Epolls, Objects, MaxEpollObjects, MaxInterestsPerEpoll,
          MaxGlobalInterests

NoRestore == "none"
RestoreCommitted == "committed"
RestoreRejected == "rejected"
RestoreStatuses == {NoRestore, RestoreCommitted, RestoreRejected}

EmptyInterests == [epoll \in Epolls |-> {}]

BatchType == [live: SUBSET Epolls, interests: [Epolls -> SUBSET Objects]]

InterestCount(interestsByEpoll) ==
    Cardinality({<<epoll, object>> \in Epolls \X Objects:
        object \in interestsByEpoll[epoll]})

BatchAdmitted(batch, currentLive) ==
    /\ batch.live \cap currentLive = {}
    /\ Cardinality(currentLive \cup batch.live) <= MaxEpollObjects
    /\ \A epoll \in batch.live:
        Cardinality(batch.interests[epoll]) <= MaxInterestsPerEpoll
    /\ \A epoll \in Epolls \ batch.live: batch.interests[epoll] = {}

\* The fixed finite candidates make both cap rejection modes reachable while
\* keeping the transactional state space small enough for the PR TLC profile.
CommittedBatch ==
    [live |-> {"epoll1"},
     interests |-> [epoll \in Epolls |->
        IF epoll = "epoll1" THEN {"object2"}
        ELSE {}]]

TooManyObjectsBatch ==
    [live |-> Epolls, interests |-> EmptyInterests]

TooManyInterestsBatch ==
    [live |-> {"epoll2"},
     interests |-> [epoll \in Epolls |->
        IF epoll = "epoll2" THEN Objects ELSE {}]]

DuplicateLiveEpollBatch ==
    [live |-> {"epoll0"},
     interests |-> [epoll \in Epolls |->
        IF epoll = "epoll0" THEN {"object2"} ELSE {}]]

RestoreBatches == {CommittedBatch, TooManyObjectsBatch, TooManyInterestsBatch,
                   DuplicateLiveEpollBatch}

VARIABLES epollLive, interests, preRestoreLive, preRestoreInterests,
          requestedRestore, lastRestoreStatus

vars == <<epollLive, interests, preRestoreLive, preRestoreInterests,
          requestedRestore, lastRestoreStatus>>

Init ==
    /\ epollLive = {}
    /\ interests = EmptyInterests
    /\ preRestoreLive = {}
    /\ preRestoreInterests = EmptyInterests
    /\ requestedRestore = [live |-> {}, interests |-> EmptyInterests]
    /\ lastRestoreStatus = NoRestore

ClearRestoreRecord(nextLive, nextInterests) ==
    /\ preRestoreLive' = nextLive
    /\ preRestoreInterests' = nextInterests
    /\ requestedRestore' = [live |-> {}, interests |-> EmptyInterests]
    /\ lastRestoreStatus' = NoRestore

CreateEpoll(epoll) ==
    /\ epoll \in Epolls \ epollLive
    /\ Cardinality(epollLive) < MaxEpollObjects
    /\ LET nextLive == epollLive \cup {epoll}
           nextInterests == [interests EXCEPT ![epoll] = {}]
       IN /\ epollLive' = nextLive
          /\ interests' = nextInterests
          /\ ClearRestoreRecord(nextLive, nextInterests)

AddInterest(epoll, object) ==
    /\ epoll \in epollLive
    /\ object \in Objects \ interests[epoll]
    /\ Cardinality(interests[epoll]) < MaxInterestsPerEpoll
    /\ LET nextInterests == [interests EXCEPT ![epoll] = @ \cup {object}]
       IN /\ interests' = nextInterests
          /\ UNCHANGED epollLive
          /\ ClearRestoreRecord(epollLive, nextInterests)

DeleteInterest(epoll, object) ==
    /\ epoll \in epollLive
    /\ object \in interests[epoll]
    /\ LET nextInterests == [interests EXCEPT ![epoll] = @ \ {object}]
       IN /\ interests' = nextInterests
          /\ UNCHANGED epollLive
          /\ ClearRestoreRecord(epollLive, nextInterests)

RetireEpoll(epoll) ==
    /\ epoll \in epollLive
    /\ LET nextLive == epollLive \ {epoll}
           nextInterests == [interests EXCEPT ![epoll] = {}]
       IN /\ epollLive' = nextLive
          /\ interests' = nextInterests
          /\ ClearRestoreRecord(nextLive, nextInterests)

RestoreAll(candidate) ==
    /\ candidate \in RestoreBatches
    /\ lastRestoreStatus = NoRestore
    /\ LET admitted == BatchAdmitted(candidate, epollLive) IN
       /\ preRestoreLive' = epollLive
       /\ preRestoreInterests' = interests
       /\ requestedRestore' = candidate
       /\ lastRestoreStatus' = IF admitted THEN RestoreCommitted
                               ELSE RestoreRejected
       /\ IF admitted
             THEN /\ epollLive' = epollLive \cup candidate.live
                  /\ interests' = [epoll \in Epolls |->
                      IF epoll \in candidate.live THEN candidate.interests[epoll]
                      ELSE interests[epoll]]
             ELSE /\ UNCHANGED <<epollLive, interests>>

Next ==
    \/ \E epoll \in Epolls: CreateEpoll(epoll)
    \/ \E epoll \in Epolls, object \in Objects: AddInterest(epoll, object)
    \/ \E epoll \in Epolls, object \in Objects: DeleteInterest(epoll, object)
    \/ \E epoll \in Epolls: RetireEpoll(epoll)
    \/ \E candidate \in RestoreBatches: RestoreAll(candidate)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ epollLive \subseteq Epolls
    /\ interests \in [Epolls -> SUBSET Objects]
    /\ preRestoreLive \subseteq Epolls
    /\ preRestoreInterests \in [Epolls -> SUBSET Objects]
    /\ requestedRestore \in BatchType
    /\ lastRestoreStatus \in RestoreStatuses

ConfiguredGlobalCapacityIsProduct ==
    MaxGlobalInterests = MaxEpollObjects * MaxInterestsPerEpoll

LiveEpollObjectsRemainBounded ==
    Cardinality(epollLive) <= MaxEpollObjects

PerEpollInterestCountsStayBounded ==
    \A epoll \in epollLive:
        Cardinality(interests[epoll]) <= MaxInterestsPerEpoll

GlobalInterestCountStaysBounded ==
    InterestCount(interests) <= MaxGlobalInterests

RetiredEpollsRetainNoInterests ==
    \A epoll \in Epolls \ epollLive: interests[epoll] = {}

RejectedRestorePreservesExactPriorState ==
    lastRestoreStatus = RestoreRejected =>
        /\ epollLive = preRestoreLive
        /\ interests = preRestoreInterests

CommittedRestoreAddsExactBatch ==
    lastRestoreStatus = RestoreCommitted =>
        /\ requestedRestore.live \cap preRestoreLive = {}
        /\ epollLive = preRestoreLive \cup requestedRestore.live
        /\ \A epoll \in preRestoreLive:
            interests[epoll] = preRestoreInterests[epoll]
        /\ \A epoll \in requestedRestore.live:
            interests[epoll] = requestedRestore.interests[epoll]

=============================================================================
