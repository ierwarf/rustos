---- MODULE IpcRegisterRendezvous ----
EXTENDS Naturals, FiniteSets

\* Objectless virtual rendezvous slot lifecycle (Track L1):
\* - Virtual tokens encode (generation, slot) with low 16 bits == 0, strictly
\*   disjoint from REPLIES generational slab handles (low 16 bits in 1..512).
\* - Exact state machine: Idle -> RequestReady -> RequestTaken ->
\*   ResponseReady | ErrorReady -> Idle, plus Rollback, Cancel, and FailOwner.
\* - Generation monotonically advances on retire and permanently retires at
\*   MaxGen (0), preventing token aliasing after completion, rollback, or cancel.

CONSTANT MaxGen

Idle == "idle"
RequestReady == "request-ready"
RequestTaken == "request-taken"
ResponseReady == "response-ready"
ErrorReady == "error-ready"
NoToken == 0

VARIABLES state, occupied, used, gen, curToken, retiredTokens,
          epFast, epDead, waiterParked, custodyHeld, terminalCount

vars == <<state, occupied, used, gen, curToken, retiredTokens,
          epFast, epDead, waiterParked, custodyHeld, terminalCount>>

SlotToken(g) == g * 65536
NextGen(g) == IF g < MaxGen THEN g + 1 ELSE 0

Init ==
    /\ state = Idle
    /\ occupied = FALSE
    /\ used = FALSE
    /\ gen = 1
    /\ curToken = NoToken
    /\ retiredTokens = {}
    /\ epFast = NoToken
    /\ epDead = FALSE
    /\ waiterParked = TRUE
    /\ custodyHeld = FALSE
    /\ terminalCount = 0

ReserveVirtual ==
    /\ ~occupied /\ ~epDead /\ waiterParked /\ epFast = NoToken /\ gen > 0
    /\ occupied' = TRUE
    /\ state' = RequestReady
    /\ used' = FALSE
    /\ curToken' = SlotToken(gen)
    /\ epFast' = SlotToken(gen)
    /\ waiterParked' = FALSE
    /\ custodyHeld' = TRUE
    /\ UNCHANGED <<gen, retiredTokens, epDead, terminalCount>>

TakeRequest ==
    /\ occupied /\ state = RequestReady /\ epFast = curToken
    /\ state' = RequestTaken
    /\ epFast' = NoToken
    /\ UNCHANGED <<occupied, used, gen, curToken, retiredTokens, epDead,
                   waiterParked, custodyHeld, terminalCount>>

CompleteReply ==
    /\ occupied /\ state = RequestTaken /\ ~used
    /\ \E nextState \in {ResponseReady, ErrorReady} :
        state' = nextState
    /\ used' = TRUE
    /\ custodyHeld' = FALSE
    /\ UNCHANGED <<occupied, gen, curToken, retiredTokens, epFast, epDead,
                   waiterParked, terminalCount>>

TakeResponse ==
    /\ occupied /\ state \in {ResponseReady, ErrorReady}
    /\ occupied' = FALSE
    /\ state' = Idle
    /\ used' = FALSE
    /\ retiredTokens' = retiredTokens \cup {curToken}
    /\ curToken' = NoToken
    /\ gen' = NextGen(gen)
    /\ terminalCount' = terminalCount + 1
    /\ UNCHANGED <<epFast, epDead, waiterParked, custodyHeld>>

Rollback ==
    /\ occupied /\ state = RequestReady /\ ~used /\ epFast = curToken
    /\ occupied' = FALSE
    /\ state' = Idle
    /\ used' = FALSE
    /\ epFast' = NoToken
    /\ waiterParked' = TRUE
    /\ custodyHeld' = FALSE
    /\ retiredTokens' = retiredTokens \cup {curToken}
    /\ curToken' = NoToken
    /\ gen' = NextGen(gen)
    /\ UNCHANGED <<epDead, terminalCount>>

Cancel ==
    /\ occupied /\ state \in {RequestReady, RequestTaken, ResponseReady, ErrorReady}
    /\ occupied' = FALSE
    /\ state' = Idle
    /\ used' = FALSE
    /\ epFast' = IF epFast = curToken THEN NoToken ELSE epFast
    /\ custodyHeld' = FALSE
    /\ retiredTokens' = retiredTokens \cup {curToken}
    /\ curToken' = NoToken
    /\ gen' = NextGen(gen)
    /\ terminalCount' = terminalCount + 1
    /\ UNCHANGED <<epDead, waiterParked>>

ReparkReceiver ==
    /\ ~occupied /\ ~waiterParked /\ ~epDead /\ gen > 0
    /\ waiterParked' = TRUE
    /\ UNCHANGED <<state, occupied, used, gen, curToken, retiredTokens,
                   epFast, epDead, custodyHeld, terminalCount>>

FailOwner ==
    /\ ~epDead
    /\ epDead' = TRUE
    /\ epFast' = NoToken
    /\ state' = IF occupied /\ ~used /\ state \in {RequestReady, RequestTaken} THEN ErrorReady ELSE state
    /\ used' = IF occupied /\ ~used /\ state \in {RequestReady, RequestTaken} THEN TRUE ELSE used
    /\ custodyHeld' = IF occupied /\ ~used /\ state \in {RequestReady, RequestTaken} THEN FALSE ELSE custodyHeld
    /\ UNCHANGED <<occupied, gen, curToken, retiredTokens, waiterParked, terminalCount>>

CompletedStop ==
    /\ ~occupied /\ (gen = 0 \/ epDead)
    /\ UNCHANGED vars

Next ==
    \/ ReparkReceiver
    \/ ReserveVirtual
    \/ TakeRequest
    \/ CompleteReply
    \/ TakeResponse
    \/ Rollback
    \/ Cancel
    \/ FailOwner
    \/ CompletedStop

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {Idle, RequestReady, RequestTaken, ResponseReady, ErrorReady}
    /\ occupied \in BOOLEAN
    /\ used \in BOOLEAN
    /\ gen \in 0..MaxGen
    /\ epDead \in BOOLEAN
    /\ waiterParked \in BOOLEAN
    /\ custodyHeld \in BOOLEAN
    /\ terminalCount \in Nat

VirtualTokenLow16Zero ==
    curToken /= NoToken => (curToken % 65536 = 0 /\ curToken \notin 1..65535)

CustodyMatchesActiveUnanswered ==
    custodyHeld <=> (occupied /\ ~used /\ state \in {RequestReady, RequestTaken})

UsedExcludesCustody ==
    used => ~custodyHeld

EndpointFastMeansReady ==
    epFast /= NoToken => (occupied /\ state = RequestReady /\ epFast = curToken)

GenerationNeverReused ==
    curToken /= NoToken => curToken \notin retiredTokens
====
