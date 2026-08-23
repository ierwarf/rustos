-------------------------- MODULE SchedulingContextBudget ---------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Tasks, Replies, Domains, RootContext, RootDomain, ServiceDomain,
          ContextBudget, DomainBudget,
          Period, MaxTime, MaxRefills, MaxTokens

NoTask == 0
NoReply == 0
Free == "free"
Reserved == "reserved"
Bound == "bound"
Replied == "replied"
Cancelled == "cancelled"
Runnable == "runnable"
Blocked == "blocked"
Exited == "exited"
NoTimeout == "none"
Throttle == "throttle"

VARIABLES clock, taskState, replyState, callerOf, receiverOf, contextOf,
          bindOrder, nextOrder,
          contextAvailable, contextPending, contextEligible, contextRefills,
          domainAvailable, domainPending, domainEligible, domainRefills,
          lastExecutor, lastContext, lastDomain,
          timeoutCount, timeoutContext, timeoutReply, timeoutAction

vars == <<clock, taskState, replyState, callerOf, receiverOf, contextOf,
          bindOrder, nextOrder,
          contextAvailable, contextPending, contextEligible, contextRefills,
          domainAvailable, domainPending, domainEligible, domainRefills,
          lastExecutor, lastContext, lastDomain,
          timeoutCount, timeoutContext, timeoutReply, timeoutAction>>

LiveReply(reply) == replyState[reply] \in {Reserved, Bound}
Min2(left, right) == IF left <= right THEN left ELSE right
Max2(left, right) == IF left >= right THEN left ELSE right
DomainOf(context) == IF context = RootContext THEN RootDomain ELSE ServiceDomain
BoundTo(reply, task) == replyState[reply] = Bound /\ receiverOf[reply] = task

NewerReplyFor(task, left, right) ==
    /\ BoundTo(left, task)
    /\ BoundTo(right, task)
    /\ bindOrder[left] > bindOrder[right]

ActiveReplies(task) == {reply \in Replies : BoundTo(reply, task)}

ActiveReply(task) ==
    IF ActiveReplies(task) = {}
    THEN NoReply
    ELSE CHOOSE reply \in ActiveReplies(task):
            \A other \in ActiveReplies(task): bindOrder[reply] >= bindOrder[other]

EffectiveContext(task) ==
    LET reply == ActiveReply(task)
    IN IF reply = NoReply THEN task ELSE contextOf[reply]

LiveTokenCount == Cardinality({reply \in Replies : LiveReply(reply)})

Init ==
    /\ Tasks # {}
    /\ Domains # {}
    /\ ContextBudget > 0 /\ DomainBudget > 0
    /\ Period > 0 /\ MaxTime > Period
    /\ MaxRefills > 0 /\ MaxTokens > 0
    /\ RootContext \in Tasks
    /\ RootDomain \in Domains /\ ServiceDomain \in Domains
    /\ clock = 0
    /\ taskState = [task \in Tasks |-> Runnable]
    /\ replyState = [reply \in Replies |-> Free]
    /\ callerOf = [reply \in Replies |-> NoTask]
    /\ receiverOf = [reply \in Replies |-> NoTask]
    /\ contextOf = [reply \in Replies |-> NoTask]
    /\ bindOrder = [reply \in Replies |-> 0]
    /\ nextOrder = 1
    /\ contextAvailable = [task \in Tasks |-> ContextBudget]
    /\ contextPending = [task \in Tasks |-> 0]
    /\ contextEligible = [task \in Tasks |-> 0]
    /\ contextRefills = [task \in Tasks |-> 0]
    /\ domainAvailable = [domain \in Domains |-> DomainBudget]
    /\ domainPending = [domain \in Domains |-> 0]
    /\ domainEligible = [domain \in Domains |-> 0]
    /\ domainRefills = [domain \in Domains |-> 0]
    /\ lastExecutor = NoTask
    /\ lastContext = NoTask
    /\ lastDomain = RootDomain
    /\ timeoutCount = 0
    /\ timeoutContext = NoTask
    /\ timeoutReply = NoReply
    /\ timeoutAction = NoTimeout

Reserve(caller, reply) ==
    /\ caller \in Tasks /\ reply \in Replies
    /\ taskState[caller] = Runnable
    /\ replyState[reply] = Free
    /\ ~\E live \in Replies: LiveReply(live) /\ callerOf[live] = caller
    /\ LiveTokenCount < MaxTokens
    /\ replyState' = [replyState EXCEPT ![reply] = Reserved]
    /\ callerOf' = [callerOf EXCEPT ![reply] = caller]
    /\ contextOf' = [contextOf EXCEPT ![reply] = EffectiveContext(caller)]
    /\ UNCHANGED <<clock, taskState, receiverOf, bindOrder, nextOrder,
                    contextAvailable, contextPending, contextEligible,
                    contextRefills, domainAvailable, domainPending,
                    domainEligible, domainRefills, lastExecutor, lastContext,
                    lastDomain, timeoutCount, timeoutContext, timeoutReply,
                    timeoutAction>>

BindReply(caller, receiver, reply) ==
    /\ caller \in Tasks /\ receiver \in Tasks /\ reply \in Replies
    /\ caller # receiver
    /\ replyState[reply] = Reserved
    /\ callerOf[reply] = caller
    /\ contextOf[reply] = EffectiveContext(caller)
    /\ taskState[caller] = Runnable
    /\ taskState[receiver] # Exited
    /\ replyState' = [replyState EXCEPT ![reply] = Bound]
    /\ receiverOf' = [receiverOf EXCEPT ![reply] = receiver]
    /\ bindOrder' = [bindOrder EXCEPT ![reply] = nextOrder]
    /\ nextOrder' = nextOrder + 1
    /\ taskState' = [taskState EXCEPT ![caller] = Blocked,
                                      ![receiver] = Runnable]
    /\ UNCHANGED <<clock, callerOf, contextOf,
                    contextAvailable, contextPending, contextEligible,
                    contextRefills, domainAvailable, domainPending,
                    domainEligible, domainRefills, lastExecutor, lastContext,
                    lastDomain, timeoutCount, timeoutContext, timeoutReply,
                    timeoutAction>>

CompleteReply(receiver, reply) ==
    /\ receiver \in Tasks /\ reply \in Replies
    /\ BoundTo(reply, receiver)
    /\ taskState[receiver] # Exited
    /\ replyState' = [replyState EXCEPT ![reply] = Replied]
    /\ taskState' = [taskState EXCEPT ![callerOf[reply]] = Runnable]
    /\ UNCHANGED <<clock, callerOf, receiverOf, contextOf, bindOrder, nextOrder,
                    contextAvailable, contextPending, contextEligible,
                    contextRefills, domainAvailable, domainPending,
                    domainEligible, domainRefills, lastExecutor, lastContext,
                    lastDomain, timeoutCount, timeoutContext, timeoutReply,
                    timeoutAction>>

CancelChain(caller, reply) ==
    /\ caller \in Tasks /\ reply \in Replies
    /\ LiveReply(reply) /\ callerOf[reply] = caller
    /\ LET owner == contextOf[reply]
           doomed == {other \in Replies : LiveReply(other)
                                          /\ contextOf[other] = owner}
       IN /\ replyState' = [other \in Replies |->
                IF other \in doomed THEN Cancelled ELSE replyState[other]]
          /\ taskState' = [task \in Tasks |->
                IF \E other \in doomed: callerOf[other] = task
                THEN Runnable ELSE taskState[task]]
    /\ UNCHANGED <<clock, callerOf, receiverOf, contextOf, bindOrder, nextOrder,
                    contextAvailable, contextPending, contextEligible,
                    contextRefills, domainAvailable, domainPending,
                    domainEligible, domainRefills, lastExecutor, lastContext,
                    lastDomain, timeoutCount, timeoutContext, timeoutReply,
                    timeoutAction>>

Charge(executor) ==
    /\ executor \in Tasks /\ taskState[executor] = Runnable
    /\ LET context == EffectiveContext(executor)
           domain == DomainOf(context)
       IN /\ contextAvailable[context] > 0
          /\ domainAvailable[domain] > 0
          /\ contextAvailable' =
                [contextAvailable EXCEPT ![context] = @ - 1]
          /\ contextPending' = [contextPending EXCEPT ![context] = @ + 1]
          /\ contextEligible' = [contextEligible EXCEPT
                ![context] = IF contextRefills[context] = 0
                             THEN clock + Period
                             ELSE Max2(@, clock + Period)]
          /\ contextRefills' = [contextRefills EXCEPT
                ![context] = Min2(@ + 1, MaxRefills)]
          /\ domainAvailable' = [domainAvailable EXCEPT ![domain] = @ - 1]
          /\ domainPending' = [domainPending EXCEPT ![domain] = @ + 1]
          /\ domainEligible' = [domainEligible EXCEPT
                ![domain] = IF domainRefills[domain] = 0
                            THEN clock + Period
                            ELSE Max2(@, clock + Period)]
          /\ domainRefills' = [domainRefills EXCEPT
                ![domain] = Min2(@ + 1, MaxRefills)]
          /\ lastExecutor' = executor
          /\ lastContext' = context
          /\ lastDomain' = domain
          /\ timeoutCount' =
                IF contextAvailable[context] = 1
                THEN timeoutCount + 1 ELSE timeoutCount
          /\ timeoutContext' =
                IF contextAvailable[context] = 1
                THEN context ELSE timeoutContext
          /\ timeoutReply' =
                IF contextAvailable[context] = 1
                THEN ActiveReply(executor) ELSE timeoutReply
          /\ timeoutAction' =
                IF contextAvailable[context] = 1
                THEN Throttle ELSE timeoutAction
    /\ UNCHANGED <<clock, taskState, replyState, callerOf, receiverOf,
                    contextOf, bindOrder, nextOrder>>

RefillContext(context) ==
    /\ context \in Tasks
    /\ contextPending[context] > 0
    /\ clock >= contextEligible[context]
    /\ contextAvailable' = [contextAvailable EXCEPT
          ![context] = @ + contextPending[context]]
    /\ contextPending' = [contextPending EXCEPT ![context] = 0]
    /\ contextEligible' = [contextEligible EXCEPT ![context] = 0]
    /\ contextRefills' = [contextRefills EXCEPT ![context] = 0]
    /\ UNCHANGED <<clock, taskState, replyState, callerOf, receiverOf,
                    contextOf, bindOrder, nextOrder, domainAvailable,
                    domainPending, domainEligible, domainRefills,
                    lastExecutor, lastContext, lastDomain, timeoutCount,
                    timeoutContext, timeoutReply, timeoutAction>>

RefillDomain(domain) ==
    /\ domain \in Domains
    /\ domainPending[domain] > 0
    /\ clock >= domainEligible[domain]
    /\ domainAvailable' = [domainAvailable EXCEPT
          ![domain] = @ + domainPending[domain]]
    /\ domainPending' = [domainPending EXCEPT ![domain] = 0]
    /\ domainEligible' = [domainEligible EXCEPT ![domain] = 0]
    /\ domainRefills' = [domainRefills EXCEPT ![domain] = 0]
    /\ UNCHANGED <<clock, taskState, replyState, callerOf, receiverOf,
                    contextOf, bindOrder, nextOrder, contextAvailable,
                    contextPending, contextEligible, contextRefills,
                    lastExecutor, lastContext, lastDomain, timeoutCount,
                    timeoutContext, timeoutReply, timeoutAction>>

Tick ==
    /\ clock < MaxTime
    /\ clock' = clock + 1
    /\ UNCHANGED <<taskState, replyState, callerOf, receiverOf, contextOf,
                    bindOrder, nextOrder, contextAvailable, contextPending,
                    contextEligible, contextRefills, domainAvailable,
                    domainPending, domainEligible, domainRefills,
                    lastExecutor, lastContext, lastDomain, timeoutCount,
                    timeoutContext, timeoutReply, timeoutAction>>

Next ==
    \/ \E caller \in Tasks, reply \in Replies: Reserve(caller, reply)
    \/ \E caller, receiver \in Tasks, reply \in Replies:
          BindReply(caller, receiver, reply)
    \/ \E receiver \in Tasks, reply \in Replies: CompleteReply(receiver, reply)
    \/ \E caller \in Tasks, reply \in Replies: CancelChain(caller, reply)
    \/ \E executor \in Tasks: Charge(executor)
    \/ \E context \in Tasks: RefillContext(context)
    \/ \E domain \in Domains: RefillDomain(domain)
    \/ Tick

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ clock \in 0..MaxTime
    /\ taskState \in [Tasks -> {Runnable, Blocked, Exited}]
    /\ replyState \in [Replies -> {Free, Reserved, Bound, Replied, Cancelled}]
    /\ callerOf \in [Replies -> (Tasks \cup {NoTask})]
    /\ receiverOf \in [Replies -> (Tasks \cup {NoTask})]
    /\ contextOf \in [Replies -> (Tasks \cup {NoTask})]
    /\ bindOrder \in [Replies -> Nat]
    /\ nextOrder \in Nat
    /\ contextAvailable \in [Tasks -> 0..ContextBudget]
    /\ contextPending \in [Tasks -> 0..ContextBudget]
    /\ contextEligible \in [Tasks -> Nat]
    /\ contextRefills \in [Tasks -> 0..MaxRefills]
    /\ domainAvailable \in [Domains -> 0..DomainBudget]
    /\ domainPending \in [Domains -> 0..DomainBudget]
    /\ domainEligible \in [Domains -> Nat]
    /\ domainRefills \in [Domains -> 0..MaxRefills]
    /\ lastExecutor \in Tasks \cup {NoTask}
    /\ lastContext \in Tasks \cup {NoTask}
    /\ lastDomain \in Domains
    /\ timeoutCount \in Nat
    /\ timeoutContext \in Tasks \cup {NoTask}
    /\ timeoutReply \in Replies \cup {NoReply}
    /\ timeoutAction \in {NoTimeout, Throttle}

ContextBudgetConserved ==
    \A context \in Tasks:
        contextAvailable[context] + contextPending[context] = ContextBudget

DomainBudgetConserved ==
    \A domain \in Domains:
        domainAvailable[domain] + domainPending[domain] = DomainBudget

CustodyIsExactAndBounded ==
    /\ LiveTokenCount <= MaxTokens
    /\ \A reply \in Replies:
          LiveReply(reply) => callerOf[reply] \in Tasks /\ contextOf[reply] \in Tasks
    /\ \A reply \in Replies:
          replyState[reply] = Bound => receiverOf[reply] \in Tasks

TerminalReplyHasNoActiveCustody ==
    \A reply \in Replies:
        replyState[reply] \in {Free, Replied, Cancelled} =>
            \A task \in Tasks: ActiveReply(task) # reply

NestedDonationKeepsRootContext ==
    \A outer, inner \in Replies:
        replyState[outer] = Bound /\ replyState[inner] = Bound
          /\ receiverOf[outer] = callerOf[inner]
          /\ ActiveReply(callerOf[inner]) = outer
          /\ bindOrder[outer] < bindOrder[inner]
          => contextOf[inner] = contextOf[outer]

LatestChargeTokenWins ==
    \A task \in Tasks:
        ActiveReply(task) # NoReply =>
          \A reply \in ActiveReplies(task):
              bindOrder[ActiveReply(task)] >= bindOrder[reply]

TimeoutFaultIsExactAndBounded ==
    /\ timeoutCount = 0 => timeoutAction = NoTimeout
    /\ timeoutCount > 0 => timeoutContext \in Tasks /\ timeoutAction = Throttle
    /\ timeoutReply = NoReply \/ timeoutReply \in Replies

=============================================================================
