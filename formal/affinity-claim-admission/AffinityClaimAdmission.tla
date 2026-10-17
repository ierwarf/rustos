----------------------- MODULE AffinityClaimAdmission -----------------------
EXTENDS Naturals, TLC
VARIABLES uid,binding,taskAllows,processAllows,pending,phase,queued,refs,leaf,
          version,snapshot,race,badClaim,badReset
vars == <<uid,binding,taskAllows,processAllows,pending,phase,queued,refs,leaf,version,snapshot,race,badClaim,badReset>>
Init == /\ uid = 1 /\ binding = 1 /\ taskAllows = TRUE /\ processAllows = TRUE
        /\ pending = FALSE /\ phase \in {"Local","Direct"} /\ queued = (phase = "Local")
        /\ refs = 0 /\ leaf = "None" /\ version = 1 /\ snapshot = 1 /\ race = FALSE
        /\ badClaim = FALSE /\ badReset = FALSE
Capture == /\ snapshot # version /\ snapshot' = version
           /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,phase,queued,refs,leaf,version,race,badClaim,badReset>>
BeginWriter == /\ leaf = "None" /\ leaf' = "Writer"
               /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,phase,queued,refs,version,snapshot,race,badClaim,badReset>>
Replace(expected,t,p) == /\ leaf = "Writer" /\ binding # 0 /\ version < 3
                         /\ IF expected = binding
                            THEN /\ taskAllows' = t /\ processAllows' = p /\ pending' = ~t
                                 /\ version' = version + 1
                                 /\ UNCHANGED <<uid,binding,phase,queued,refs,leaf,snapshot,race,badClaim,badReset>>
                            ELSE UNCHANGED vars
EndWriter == /\ leaf = "Writer" /\ leaf' = "None"
             /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,phase,queued,refs,version,snapshot,race,badClaim,badReset>>
AcquireClaim == /\ leaf = "None" /\ phase \in {"Local","Direct"} /\ leaf' = "Claim"
                /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,phase,queued,refs,version,snapshot,race,badClaim,badReset>>
Eligible == taskAllows /\ processAllows /\ binding = uid
Claim == /\ leaf = "Claim" /\ Eligible /\ ~race
         /\ phase' = "Running" /\ queued' = FALSE /\ refs' = 1 /\ leaf' = "None"
         /\ badClaim' = (badClaim \/ ~taskAllows \/ ~processAllows \/ binding # uid)
         /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,version,snapshot,race,badReset>>
Refuse == /\ leaf = "Claim" /\ (~Eligible \/ race) /\ leaf' = "None"
          /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,phase,queued,refs,version,snapshot,race,badClaim,badReset>>
RaceOwner == /\ phase \in {"Local","Direct"} /\ ~race /\ race' = TRUE
             /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,phase,queued,refs,leaf,version,snapshot,badClaim,badReset>>
Retire == /\ leaf = "None" /\ phase \in {"Local","Direct","Running"}
          /\ phase' = "Retired" /\ queued' = FALSE
          /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,refs,leaf,version,snapshot,race,badClaim,badReset>>
Release == /\ refs = 1 /\ phase = "Retired" /\ refs' = 0
           /\ UNCHANGED <<uid,binding,taskAllows,processAllows,pending,phase,queued,leaf,version,snapshot,race,badClaim,badReset>>
Reset == /\ leaf = "Writer" /\ refs = 0 /\ phase = "Retired"
         /\ binding' = 0 /\ taskAllows' = TRUE /\ processAllows' = TRUE /\ pending' = FALSE
         /\ phase' = "Dormant" /\ badReset' = (badReset \/ refs # 0)
         /\ UNCHANGED <<uid,queued,refs,leaf,version,snapshot,race,badClaim>>
Rebind(nextPhase) == /\ leaf = "Writer" /\ binding = 0 /\ uid = 1 /\ phase = "Dormant"
                     /\ uid' = 2 /\ binding' = 2 /\ phase' = nextPhase /\ queued' = (nextPhase = "Local")
                     /\ race' = FALSE
                     /\ UNCHANGED <<taskAllows,processAllows,pending,refs,leaf,version,snapshot,badClaim,badReset>>
Next == Capture \/ BeginWriter \/ (\E t,p \in BOOLEAN: Replace(binding,t,p))
        \/ Replace(1,FALSE,FALSE) \/ EndWriter \/ AcquireClaim \/ Claim \/ Refuse
        \/ RaceOwner \/ Retire \/ Release \/ Reset \/ Rebind("Local") \/ Rebind("Direct") \/ UNCHANGED vars
TypeOK == /\ uid \in {1,2} /\ binding \in {0,1,2} /\ version \in 1..3 /\ snapshot \in 1..3
          /\ phase \in {"Local","Direct","Running","Retired","Dormant"} /\ leaf \in {"None","Claim","Writer"}
          /\ refs \in {0,1} /\ <<taskAllows,processAllows,pending,queued,race,badClaim,badReset>> \in BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN
ClaimHasCurrentAffinity == ~badClaim
ResetWaitsExecution == ~badReset
MembershipPreserved == queued = (phase = "Local")
ClaimLeafExclusive == leaf = "Claim" => phase \in {"Local","Direct"}
=============================================================================
