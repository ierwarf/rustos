--------------------- MODULE LifecycleExecutionClaim ---------------------
EXTENDS Naturals, TLC
VARIABLES uid, bound, visible, stopped, gen, snapshot, phase, queued,
          held, prepared, current, outgoing, writer, badClaim, badReset
baseVars == <<uid, bound, visible, stopped, gen, snapshot, phase, queued,
          held, prepared, current, outgoing, writer, badClaim, badReset>>
Refs == (IF held THEN 1 ELSE 0)
        + (IF prepared THEN 1 ELSE 0)
        + (IF current THEN 1 ELSE 0)
        + (IF outgoing THEN 1 ELSE 0)
BaseInit == /\ uid = 1 /\ bound = 1 /\ visible = TRUE /\ stopped = FALSE
        /\ gen = 1 /\ snapshot = 1 /\ phase = "Local" /\ queued = TRUE
        /\ held = FALSE /\ prepared = FALSE /\ current = FALSE /\ outgoing = FALSE
        /\ writer = FALSE /\ badClaim = FALSE /\ badReset = FALSE
Capture == /\ phase = "Local" /\ visible /\ ~stopped
           /\ held' = TRUE /\ writer' = TRUE /\ snapshot' = gen /\ phase' = "Claiming"
           /\ UNCHANGED <<uid,bound,visible,stopped,gen,queued,prepared,current,outgoing,badClaim,badReset>>
Republish == /\ phase = "Claiming" /\ gen = 1 /\ gen' = 2
             /\ UNCHANGED <<uid,bound,visible,stopped,snapshot,phase,queued,held,prepared,current,outgoing,writer,badClaim,badReset>>
Claim == /\ phase = "Claiming" /\ gen = snapshot
         /\ phase' = "Claimed" /\ queued' = FALSE /\ writer' = FALSE
         /\ badClaim' = (badClaim \/ stopped \/ ~visible)
         /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,held,prepared,current,outgoing,badReset>>
Refuse == /\ phase = "Claiming" /\ gen # snapshot
          /\ phase' = "Local" /\ held' = FALSE /\ writer' = FALSE
          /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,queued,prepared,current,outgoing,badClaim,badReset>>
Stop == /\ ~writer /\ ~stopped /\ stopped' = TRUE
        /\ UNCHANGED <<uid,bound,visible,gen,snapshot,phase,queued,held,prepared,current,outgoing,writer,badClaim,badReset>>
Hide == /\ ~writer /\ visible /\ visible' = FALSE
        /\ UNCHANGED <<uid,bound,stopped,gen,snapshot,phase,queued,held,prepared,current,outgoing,writer,badClaim,badReset>>
Prepare == /\ phase = "Claimed" /\ phase' = "Prepared" /\ held' = FALSE /\ prepared' = TRUE
           /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,queued,current,outgoing,writer,badClaim,badReset>>
Publish == /\ phase = "Prepared" /\ phase' = "Current" /\ prepared' = FALSE /\ current' = TRUE
           /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,queued,held,outgoing,writer,badClaim,badReset>>
Switch == /\ phase = "Current" /\ phase' = "AwaitingRsp" /\ current' = FALSE /\ outgoing' = TRUE
          /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,queued,held,prepared,writer,badClaim,badReset>>
BeginAck == /\ phase = "AwaitingRsp" /\ phase' = "Committing"
            /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,queued,held,prepared,current,outgoing,writer,badClaim,badReset>>
Ack == /\ phase = "Committing" /\ phase' = "Done" /\ outgoing' = FALSE
       /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,queued,held,prepared,current,writer,badClaim,badReset>>
Poison == /\ phase \in {"Claimed","Prepared","Committing"} /\ phase' = "Poisoned"
          /\ UNCHANGED <<uid,bound,visible,stopped,gen,snapshot,queued,held,prepared,current,outgoing,writer,badClaim,badReset>>
Reset == /\ ~writer /\ Refs = 0 /\ uid = 1 /\ uid' = 2 /\ bound' = 2
         /\ visible' = FALSE /\ queued' = FALSE /\ phase' = "Vacant"
         /\ badReset' = (badReset \/ (Refs # 0))
         /\ UNCHANGED <<stopped,gen,snapshot,held,prepared,current,outgoing,writer,badClaim>>
BaseNext == Capture \/ Republish \/ Claim \/ Refuse \/ Stop \/ Hide \/ Prepare \/ Publish \/ Switch \/ BeginAck \/ Ack \/ Poison \/ Reset \/ UNCHANGED baseVars
TypeOK == /\ uid \in {1,2} /\ bound \in {1,2} /\ gen \in {1,2} /\ snapshot \in {1,2}
          /\ phase \in {"Local","Claiming","Claimed","Prepared","Current","AwaitingRsp","Committing","Done","Poisoned","Vacant"}
          /\ <<visible,stopped,queued,held,prepared,current,outgoing,writer,badClaim,badReset>> \in BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN
ClaimIntentSerialized == ~badClaim
IdentityResetDrained == ~badReset
BankConservation == /\ Refs \in {0,1} /\ (Refs > 0 => uid = bound)
                    /\ (phase \in {"Claiming","Claimed","Prepared","Current","AwaitingRsp","Committing","Poisoned"} => Refs = 1)
WriterMatchesLeaf == writer = (phase = "Claiming")
OutgoingUntilAck == (phase \in {"AwaitingRsp","Committing"}) => outgoing

(*******************************************************************************
Original Current cold effects refinement of current_terminal.rs, the two parked
banks and execution_interval::CurrentInterval. Earlier successful physical claim
and FundingPin capture are abstract inputs, not an unsafe generation relookup.
The same Current permit and physical/MM bank remain parked after terminal close;
a unique non-Copy interval borrow excludes settlement through the last effect.
No leaf/raw guard is retained over recipient work. This is NOT complete priority,
architecture, hardware RSP, process-census or shared-MM/exec joint refinement.
Original base actions, invariants and mutation obligations remain registered.
*******************************************************************************)
VARIABLES physicalCurrent, originalFunding, activeInterval, currentBorrow,
          terminalEffects, effectsDone, settledCount, earlySettle
coldVars == <<physicalCurrent, originalFunding, activeInterval, currentBorrow,
              terminalEffects, effectsDone, settledCount, earlySettle>>
currentVars == <<baseVars, coldVars>>
\* Runtime observation holds the SAME Current interval/funding loan while later
\* donation metadata can change. Scalars below describe that loan, not new pins.
VARIABLES originalOwner, donationOwner, snapshotOwner
observationVars == <<originalOwner, donationOwner, snapshotOwner>>
componentVars == <<currentVars, observationVars>>
ComponentInit == /\ BaseInit
        /\ physicalCurrent = FALSE /\ originalFunding = FALSE
        /\ activeInterval = FALSE /\ currentBorrow = FALSE
        /\ terminalEffects = FALSE /\ effectsDone = FALSE
        /\ settledCount = 0 /\ earlySettle = FALSE
        /\ originalOwner = 0 /\ donationOwner = 1 /\ snapshotOwner = 0
OpenOriginalClaim == /\ Claim /\ originalFunding' = TRUE /\ activeInterval' = TRUE
                     /\ UNCHANGED <<physicalCurrent,currentBorrow,terminalEffects,effectsDone,settledCount,earlySettle>>
PublishOriginalBanks == /\ Publish /\ physicalCurrent' = TRUE
                        /\ UNCHANGED <<originalFunding,activeInterval,currentBorrow,terminalEffects,effectsDone,settledCount,earlySettle>>
BorrowCurrent == /\ current /\ physicalCurrent /\ originalFunding /\ activeInterval
                 /\ ~currentBorrow /\ currentBorrow' = TRUE
                 /\ UNCHANGED <<baseVars,physicalCurrent,originalFunding,activeInterval,terminalEffects,effectsDone,settledCount,earlySettle>>
StopCurrent == /\ currentBorrow /\ ~terminalEffects
               /\ stopped' = TRUE /\ terminalEffects' = TRUE
               /\ UNCHANGED <<uid,bound,visible,gen,snapshot,phase,queued,held,prepared,current,outgoing,writer,badClaim,badReset>>
               /\ UNCHANGED <<physicalCurrent,originalFunding,activeInterval,currentBorrow,effectsDone,settledCount,earlySettle>>
LastCurrentEffect == /\ currentBorrow /\ currentBorrow' = FALSE
                     /\ effectsDone' = terminalEffects
                     /\ UNCHANGED <<baseVars,physicalCurrent,originalFunding,activeInterval,terminalEffects,settledCount,earlySettle>>
SettleOriginal == /\ current /\ activeInterval /\ ~currentBorrow
                  /\ activeInterval' = FALSE /\ originalFunding' = FALSE
                  /\ settledCount' = settledCount + 1
                  /\ earlySettle' = (earlySettle \/ currentBorrow)
                  /\ UNCHANGED <<baseVars,physicalCurrent,currentBorrow,terminalEffects,effectsDone>>
SwitchOriginal == /\ ~currentBorrow /\ ~activeInterval /\ Switch
                  /\ UNCHANGED coldVars
AckOriginal == /\ Ack /\ physicalCurrent' = FALSE
               /\ UNCHANGED <<originalFunding,activeInterval,currentBorrow,terminalEffects,effectsDone,settledCount,earlySettle>>
\* Reset's ORIGINAL Refs=0 admission is the sole identity-reset authority.
\* The invariants independently require MM/funding/interval/effect loans drained;
\* redundant wrapper guards must not conceal a broken original reset admission.
ResetOriginal == /\ Reset /\ UNCHANGED coldVars
ColdNext == \/ ((Capture \/ Republish \/ Refuse \/ Stop \/ Hide \/ Prepare \/ BeginAck \/ Poison) /\ UNCHANGED coldVars)
            \/ PublishOriginalBanks \/ BorrowCurrent \/ StopCurrent
            \/ LastCurrentEffect \/ SettleOriginal \/ SwitchOriginal \/ AckOriginal \/ ResetOriginal
            \/ UNCHANGED currentVars
CaptureOriginalOwner == /\ OpenOriginalClaim /\ originalOwner' = donationOwner
                        /\ UNCHANGED <<donationOwner, snapshotOwner>>
ChangeDonationMetadata == /\ originalFunding /\ donationOwner = 1 /\ donationOwner' = 2
                          /\ UNCHANGED <<currentVars, originalOwner, snapshotOwner>>
ReadOriginalFunding == /\ currentBorrow /\ snapshotOwner = 0
                      /\ snapshotOwner' = originalOwner
                      /\ UNCHANGED <<currentVars, originalOwner, donationOwner>>
ComponentNext == \/ (ColdNext /\ UNCHANGED observationVars)
        \/ CaptureOriginalOwner \/ ChangeDonationMetadata \/ ReadOriginalFunding
ObservationTypeOK == /\ originalOwner \in {0,1} /\ donationOwner \in {1,2}
                     /\ snapshotOwner \in {0,1,2}
OriginalFundingObservation == /\ (originalFunding => originalOwner = 1)
                             /\ (snapshotOwner # 0 => snapshotOwner = originalOwner)
ColdTypeOK == /\ <<physicalCurrent,originalFunding,activeInterval,currentBorrow,terminalEffects,effectsDone,earlySettle>>
                   \in BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN
              /\ settledCount \in {0,1}
CurrentOriginalUnits == currentBorrow => (current /\ physicalCurrent /\ originalFunding /\ activeInterval /\ uid = bound)
OriginalMmThroughRsp == (current \/ outgoing) => physicalCurrent
OriginalFundingUntilSettlement == activeInterval = originalFunding
LastEffectBeforeSettlement == ~earlySettle
ColdReuseDrained == uid = 2 => (~physicalCurrent /\ ~originalFunding /\ ~activeInterval /\ ~currentBorrow)
(*******************************************************************************
Source refinement of the bounded original IRQ return/terminal path:
 Choice/Capture/Fund -> DispatchFrame::prepare/claim with original physical/MM
 and UID loans -> ReturnArchitecture::install_with effects -> publish_return's
 strict resource qualification -> BOTH bank publications -> unique RSP callback
 -> deferred original terminal producer -> external/kernel ACK -> sealed reuse.
 Privileged effects and arrival on incoming RSP are EXTERNAL inputs, not hardware
 acceptance. SAME Current restores only SIMD; changed Current retains outgoing
 source bank and UID until callback. Incoming Current remains retained at this
 model's end; this does NOT prove its next switch, shared-MM exec/census, foreign
 CPU admission, or the complete global-deletion requirement. Existing component
 actions and all their mutation obligations remain independently exercised.
*******************************************************************************)
VARIABLE j
jointVars == <<j>>
VARIABLES c, f, proofMode
vars == <<componentVars, jointVars, c, f, proofMode>>
JointInit == j = [phase |-> "Entry", same |-> FALSE, choice |-> FALSE,
    framePin |-> FALSE, fresh |-> TRUE, prepared |-> FALSE, uidPermit |-> FALSE,
    mmPin |-> FALSE, sourceBank |-> TRUE, outgoing |-> FALSE, currentTo |-> FALSE,
    active |-> FALSE, funding |-> FALSE, donor |-> 0, metadataDonor |-> 1,
    effects |-> 0, charged |-> 0, stopSource |-> FALSE, sourceEffects |-> FALSE,
    externalAck |-> FALSE, kernelAck |-> FALSE, reused |-> FALSE, sourceUid |-> 1,
    badReuse |-> FALSE, badPublish |-> FALSE, badCharge |-> FALSE, badEffect |-> FALSE]
JChoose == /\ j.phase = "Entry"
           /\ \E same \in BOOLEAN: j' = [j EXCEPT !.phase = "Chosen", !.choice = TRUE, !.same = same]
JCapture == /\ j.phase = "Chosen" /\ j.choice
            /\ j' = [j EXCEPT !.phase = "Captured", !.framePin = TRUE, !.mmPin = TRUE]
JRepublish == /\ j.phase = "Captured" /\ j.fresh
              /\ j' = [j EXCEPT !.fresh = FALSE]
JFund == /\ j.phase = "Captured" /\ j.fresh /\ j.framePin /\ j.mmPin
         /\ j' = [j EXCEPT !.phase = "Funded", !.funding = TRUE, !.donor = j.metadataDonor]
JRefuse == /\ j.phase = "Captured" /\ ~j.fresh
           /\ j' = [j EXCEPT !.phase = "Refused", !.framePin = FALSE, !.mmPin = FALSE]
JClaim == /\ j.phase = "Funded" /\ j.fresh /\ j.framePin /\ j.mmPin /\ j.funding
          /\ j' = [j EXCEPT !.phase = "Claimed", !.framePin = FALSE,
                           !.prepared = TRUE, !.uidPermit = TRUE, !.active = TRUE]
JMetadata == /\ j.funding /\ j.metadataDonor = 1
             /\ j' = [j EXCEPT !.metadataDonor = 2]
JBeginInstall == /\ j.phase = "Claimed" /\ j.active /\ j.prepared /\ j.mmPin /\ j.uidPermit
                 /\ j' = [j EXCEPT !.phase = "Installing"]
RequiredEffects == IF j.same THEN 1 ELSE 4
JEffect == /\ j.phase = "Installing" /\ j.effects < RequiredEffects
           /\ j' = [j EXCEPT !.effects = @ + 1,
                !.badEffect = (@ \/ ~j.active \/ ~j.mmPin \/ ~j.prepared \/ ~j.uidPermit \/ ~j.sourceBank)]
JReady == /\ j.phase = "Installing" /\ j.effects = RequiredEffects
          /\ j' = [j EXCEPT !.phase = "Ready"]
JPublish == /\ j.phase = "Ready" /\ j.active /\ j.prepared /\ j.mmPin /\ j.uidPermit
            /\ j' = [j EXCEPT !.phase = IF j.same THEN "Current" ELSE "AwaitingRsp",
                  !.prepared = FALSE, !.sourceBank = j.same, !.outgoing = ~j.same,
                  !.currentTo = TRUE, !.choice = FALSE,
                  !.badPublish = (@ \/ j.effects # RequiredEffects \/ ~j.active)]
JAckBegin == /\ j.phase = "AwaitingRsp"
             /\ j' = [j EXCEPT !.phase = "Committing"]
JAckEnd == /\ j.phase = "Committing"
           /\ j' = [j EXCEPT !.phase = "Current", !.outgoing = FALSE]
JStopSource == /\ j.phase \in {"AwaitingRsp", "Committing", "Current"} /\ ~j.stopSource
               /\ j' = [j EXCEPT !.stopSource = TRUE]
JSourceEffects == /\ j.stopSource /\ ~j.sourceEffects /\ ~j.sourceBank /\ ~j.outgoing
                  /\ j' = [j EXCEPT !.sourceEffects = TRUE]
JExternalAck == /\ j.sourceEffects /\ ~j.externalAck
                /\ j' = [j EXCEPT !.externalAck = TRUE]
JKernelAck == /\ j.sourceEffects /\ ~j.kernelAck
              /\ j' = [j EXCEPT !.kernelAck = TRUE]
JSettle == /\ j.phase = "Current" /\ j.active /\ j.funding
           /\ j' = [j EXCEPT !.active = FALSE, !.funding = FALSE, !.charged = @ + 1,
                             !.badCharge = (@ \/ j.donor # 1)]
JReset == /\ j.sourceEffects /\ j.externalAck /\ j.kernelAck
          /\ ~j.sourceBank /\ ~j.outgoing /\ ~j.funding /\ ~j.reused
          /\ j' = [j EXCEPT !.reused = TRUE, !.sourceUid = 2,
                 !.badReuse = (@ \/ j.sourceBank \/ j.outgoing \/ j.funding \/ ~j.externalAck \/ ~j.kernelAck)]
JointNext == JChoose \/ JCapture \/ JRepublish \/ JFund \/ JRefuse \/ JClaim \/ JMetadata
             \/ JBeginInstall \/ JEffect \/ JReady \/ JPublish \/ JAckBegin \/ JAckEnd
             \/ JStopSource \/ JSourceEffects \/ JExternalAck \/ JKernelAck \/ JSettle \/ JReset
JointTypeOK == /\ j.phase \in {"Entry","Chosen","Captured","Refused","Funded","Claimed","Installing","Ready","AwaitingRsp","Committing","Current"}
               /\ j.effects \in 0..4 /\ j.charged \in 0..1 /\ j.sourceUid \in {1,2}
JointOriginalEffects == ~j.badEffect /\ ~j.badPublish
JointLastEffect == (j.phase \in {"Ready","AwaitingRsp","Committing","Current"}) => j.effects = RequiredEffects
JointOriginalFunding == /\ (j.active => j.funding) /\ ~j.badCharge
                        /\ (j.funding /\ ~j.active => j.phase = "Funded")
                        /\ (j.funding => j.donor = 1)
JointOriginalBanks == /\ (j.phase \in {"Claimed","Installing","Ready"} => (j.prepared /\ j.mmPin /\ j.uidPermit /\ j.sourceBank))
                     /\ (j.currentTo => (j.mmPin /\ j.uidPermit))
                     /\ (j.phase \in {"AwaitingRsp","Committing"} => j.outgoing)
JointTerminalDeferred == j.sourceEffects => (~j.sourceBank /\ ~j.outgoing)
JointSealedReuse == /\ ~j.badReuse
                   /\ (j.reused => (~j.sourceBank /\ ~j.outgoing /\ ~j.funding /\ j.externalAck /\ j.kernelAck /\ j.sourceUid = 2))
(*******************************************************************************
Cross-root four-return refinement of irq_entry_tests/cross_root.rs. Original
Current/Prepared/Transition physical+MM and UID banks, interval settlement and
last CR3/stack/segment/SIMD effect are one protocol, not independent component
successes. Slots 1/2 are independently born processes (different MM); slot 0 is
the earlier kernel executor with NO process activation pin. First call donates
1 -> 2; reverse reply revokes metadata while the SAME active funder remains 1.
Both born tasks leave through actual returns before external/kernel cleanup ACK;
then slot 1 is born with UID 3 and a new root while kernel Current is retained.
Only register effects, page-table roots, Online topology, RSP arrival and external
consumer ACK are modeled inputs. This is NOT foreign-CPU/shared-MM exec/census or
hardware acceptance. proofMode separates bounded state spaces, not obligations:
all earlier component actions/invariants/mutations remain reachable unchanged.
*******************************************************************************)
CycleInit == c = [phase |-> "Entry", turn |-> 1, from |-> 1, to |-> 2,
    physical |-> 1, uidCurrent |-> 1, prepared |-> 3, outgoing |-> 3, uidOutgoing |-> 3,
    frame |-> FALSE, activation |-> FALSE, placement |-> FALSE, active |-> TRUE,
    funder |-> 1, metadata |-> 1, revoked |-> FALSE, effects |-> 0,
    charged |-> [t \in 1..4 |-> 0], stopped |-> {}, currentStopped |-> FALSE,
    blockedOwner |-> 1, blockedHeader |-> 1, external |-> FALSE, kernelAck |-> FALSE,
    reused |-> FALSE, born |-> FALSE, uid |-> 1, root |-> 1,
    badCharge |-> FALSE, badEffect |-> FALSE, badPublish |-> FALSE,
    badHeader |-> FALSE, badReuse |-> FALSE, badKernel |-> FALSE]
CycleAccountOwner == IF c.turn \in {1,2,3} THEN 1 ELSE 2
CycleNewFunder == IF c.turn \in {1,2} THEN 1 ELSE IF c.turn = 3 THEN 2 ELSE 0
CRevoke == /\ c.turn = 2 /\ c.phase = "Entry" /\ ~c.revoked
           /\ c' = [c EXCEPT !.metadata = 2, !.revoked = TRUE]
CStop == /\ c.turn \in {3,4} /\ c.phase = "Entry" /\ ~c.currentStopped
         /\ c' = [c EXCEPT !.stopped = @ \cup {c.from}, !.currentStopped = TRUE]
CAccount == /\ c.phase = "Entry" /\ c.active
            /\ (c.turn = 2 => c.revoked /\ c.blockedHeader = c.blockedOwner)
            /\ (c.turn \in {3,4} => c.currentStopped)
            /\ c' = [c EXCEPT !.phase = "Accounted", !.active = FALSE,
                 !.charged[c.turn] = @ + 1,
                 !.badCharge = (@ \/ c.funder # CycleAccountOwner),
                 !.blockedOwner = IF c.turn = 1 THEN 2 ELSE @,
                 !.blockedHeader = IF c.turn = 1 THEN 2 ELSE @]
CCapture == /\ c.phase = "Accounted" /\ c.physical = c.from /\ c.uidCurrent = c.from
            /\ c' = [c EXCEPT !.phase = "Captured", !.frame = TRUE,
                    !.activation = (c.to # 0), !.placement = TRUE]
CClaim == /\ c.phase = "Captured" /\ c.frame /\ c.placement
          /\ c' = [c EXCEPT !.phase = "Installing", !.frame = FALSE,
                  !.prepared = c.to, !.active = TRUE, !.funder = CycleNewFunder]
CEffect == /\ c.phase = "Installing" /\ c.effects < 5
           /\ c' = [c EXCEPT !.effects = @ + 1,
                !.badEffect = (@ \/ c.physical # c.from \/ c.uidCurrent # c.from
                    \/ c.prepared # c.to \/ ~c.placement \/ ~c.active),
                !.badKernel = (@ \/ (c.activation # (c.to # 0)))]
CReady == /\ c.phase = "Installing" /\ c.effects = 5
          /\ c' = [c EXCEPT !.phase = "Ready"]
CPublish == /\ c.phase = "Ready" /\ c.active /\ c.prepared = c.to
            /\ c' = [c EXCEPT !.phase = "AwaitingRsp", !.physical = c.to,
                 !.uidCurrent = c.to, !.outgoing = c.from, !.uidOutgoing = c.from,
                 !.prepared = 3, !.badPublish = (@ \/ c.effects # 5)]
CBeginAck == /\ c.phase = "AwaitingRsp"
             /\ c' = [c EXCEPT !.phase = "Committing"]
CEndAck == /\ c.phase = "Committing"
           /\ c' = [c EXCEPT !.phase = "Acknowledged", !.outgoing = 3, !.uidOutgoing = 3,
                    !.badHeader = (@ \/ (c.turn = 1 /\ c.blockedHeader # c.blockedOwner))]
CNextTurn == /\ c.phase = "Acknowledged" /\ c.turn < 4
             /\ c' = [c EXCEPT !.phase = "Entry", !.turn = @ + 1, !.from = c.to,
                   !.to = IF c.turn = 1 THEN 1 ELSE IF c.turn = 2 THEN 2 ELSE 0,
                   !.currentStopped = FALSE, !.activation = FALSE,
                   !.placement = FALSE, !.effects = 0]
CCleanupAck == /\ c.turn = 4 /\ c.phase = "Acknowledged"
               /\ c.stopped = {1,2} /\ c.outgoing = 3 /\ c.uidOutgoing = 3
               /\ c' = [c EXCEPT !.phase = "Cleanup", !.external = TRUE, !.kernelAck = TRUE]
CReset == /\ c.phase = "Cleanup" /\ c.external /\ c.kernelAck
          /\ c.physical = 0 /\ c.uidCurrent = 0 /\ c.outgoing = 3 /\ c.uidOutgoing = 3
          /\ c.funder = 0 /\ (\A t \in 1..4: c.charged[t] = 1)
          /\ c' = [c EXCEPT !.phase = "Vacant", !.reused = TRUE, !.uid = 0,
                 !.badReuse = (@ \/ ~c.external \/ ~c.kernelAck \/ c.funder # 0
                     \/ c.outgoing # 3 \/ c.uidOutgoing # 3 \/ c.physical # 0)]
CBirth == /\ c.phase = "Vacant" /\ c.reused /\ c.active /\ c.physical = 0
          /\ c' = [c EXCEPT !.phase = "Born", !.born = TRUE, !.uid = 3, !.root = 3]
CNewReset == /\ c.phase = "Born" /\ c.uid = 3
             /\ c' = [c EXCEPT !.phase = "Done", !.uid = 0]
CycleNext == CRevoke \/ CStop \/ CAccount \/ CCapture \/ CClaim \/ CEffect \/ CReady
             \/ CPublish \/ CBeginAck \/ CEndAck \/ CNextTurn \/ CCleanupAck
             \/ CReset \/ CBirth \/ CNewReset \/ UNCHANGED c
CycleTypeOK == /\ c.turn \in 1..4 /\ c.effects \in 0..5
               /\ c.uid \in {0,1,3} /\ c.root \in {1,3}
               /\ c.physical \in 0..2 /\ c.uidCurrent \in 0..2
               /\ c.outgoing \in 0..3 /\ c.uidOutgoing \in 0..3
CycleOriginalFunding == ~c.badCharge /\ \A t \in 1..4: c.charged[t] \in 0..1
CycleOriginalEffects == /\ ~c.badEffect /\ ~c.badKernel /\ ~c.badPublish
    /\ (c.phase \in {"Ready","AwaitingRsp","Committing","Acknowledged","Cleanup","Vacant","Born","Done"} => c.effects = 5)
CycleOriginalBanks == /\ c.physical = c.uidCurrent /\ c.outgoing = c.uidOutgoing
    /\ (c.phase \in {"Installing","Ready"} => (c.physical = c.from /\ c.prepared = c.to /\ c.active))
    /\ (c.phase \in {"AwaitingRsp","Committing"} => c.outgoing = c.from)
CycleBlockedHeader == ~c.badHeader
CycleReuse == /\ ~c.badReuse
    /\ (c.reused => (c.physical = 0 /\ c.uidCurrent = 0 /\ c.outgoing = 3 /\ c.uidOutgoing = 3 /\ c.funder = 0 /\ c.external /\ c.kernelAck))
    /\ (c.born => (c.reused /\ c.root = 3 /\ c.uid \in {0,3}))
(*******************************************************************************
Two actual CPU actors refine irq_entry_tests/foreign_shared.rs. Only architectural
CPU identity/register effects/RSP arrival and RTC/IPC external completion are
inputs. Original remote-mailbox/claim banks, frozen census, terminal producer,
process detach, exec staged MM/publication and resource reset remain actual
owners in that witness. A pending first RSP, Current sibling and outgoing RSP
are ALL nonquiescent. Source's old MM and active interval remain through foreign
return/ACK/detach; only then publish the new self-exec MM. Native CPU admission
of the two earlier kernel executors is explicit, not hardware acceptance.
Selection/readers, original frame+funding admission under class/Context/Domain
leaves, stale queue/class/credit/stop interference, postclaim donor metadata,
independent peer/source runtime and kernel ACKs and original reaper/new birth
are now IN this same two-actor state space. No guarded/global action supplies
its selection, claim, MM/UID bank, interval, census, ACK or constructor unit.
*******************************************************************************)
ForeignInit == f = [phase |-> "Local", current |-> 0, uidCurrent |-> 0,
    prepared |-> 3, outgoing |-> 3, uidOutgoing |-> 3, effects |-> 0,
    peerPin |-> FALSE, peerActive |-> FALSE, peerCharged |-> 0, kernel1Active |-> TRUE,
    sourceActive |-> TRUE, sourceCharged |-> 0, sourceBank |-> TRUE, kernel0Active |-> FALSE,
    census |-> FALSE, stopped |-> FALSE, detached |-> FALSE, kernelAck |-> FALSE,
    runtimeAck |-> FALSE, sourcePublished |-> FALSE, sourceRuntimeAck |-> FALSE, sourceKernelAck |-> FALSE,
    published |-> FALSE, oldRetired |-> FALSE, peerReset |-> FALSE, sourceReset |-> FALSE,
    sourceMM |-> 1, peerMM |-> 1, badEffect |-> FALSE, badCensus |-> FALSE,
    badDetach |-> FALSE, badReuse |-> FALSE, badCharge |-> FALSE,
    selection |-> "Empty", selectPin |-> FALSE, reader |-> FALSE,
    fundingGrant |-> FALSE, claimLeaf |-> FALSE, admitted |-> FALSE,
    classEpoch |-> 0, observedClass |-> 0, queueGeneration |-> 1, observedGeneration |-> 1,
    contextCredit |-> TRUE, domainCredit |-> TRUE, badAdmission |-> FALSE, stoppedAtClaim |-> FALSE,
    originalContext |-> 0, borrowedContext |-> 0, chargedContext |-> 0,
    birthTicket |-> FALSE, slotUID |-> 1, birthNonce |-> 1, born |-> FALSE,
    birthIdentity |-> 0, birthAffinity |-> 0,
    newExternal |-> FALSE, newKernel |-> FALSE, ackNonce |-> 0, oldAckTried |-> FALSE,
    newProducer |-> 0]
FRemote == /\ f.phase = "Local"
           /\ f' = [f EXCEPT !.phase = "Remote"]
\* The SAME foreign return protocol now starts at original selection/reader,
\* retained frame/funding, class+Context/Domain+resource admission and owner CAS.
\* Class/budget/queue producers race before the join, never inside its leaves.
FSelect == /\ f.phase = "Remote" /\ f.selection = "Empty" /\ ~f.stopped
           /\ f' = [f EXCEPT !.selection = "Reading", !.selectPin = TRUE, !.reader = TRUE,
                    !.observedClass = f.classEpoch, !.observedGeneration = f.queueGeneration]
FObserve == /\ f.phase = "Remote" /\ f.selection = "Reading" /\ f.selectPin /\ f.reader
            /\ f' = [f EXCEPT !.selection = "Observed", !.reader = FALSE]
FFund == /\ f.phase = "Remote" /\ f.selection = "Observed" /\ f.selectPin
         /\ ~f.stopped /\ f.contextCredit /\ f.domainCredit
         /\ f' = [f EXCEPT !.selection = "Funded", !.fundingGrant = TRUE, !.originalContext = 2]
FAdmission == f.selectPin /\ ~f.reader /\ f.fundingGrant /\ ~f.stopped
              /\ f.classEpoch = f.observedClass /\ f.queueGeneration = f.observedGeneration
              /\ f.contextCredit /\ f.domainCredit /\ f.originalContext = 2
FJoin == /\ f.phase = "Remote" /\ f.selection = "Funded" /\ ~f.claimLeaf
         /\ f' = [f EXCEPT !.selection = "Locked", !.claimLeaf = TRUE, !.admitted = FAdmission]
FClaim == /\ f.phase = "Remote"
          /\ f.selection = "Locked" /\ f.claimLeaf /\ f.admitted
          /\ f' = [f EXCEPT !.phase = "IncomingEffects", !.prepared = 2,
                           !.peerPin = TRUE, !.peerActive = TRUE, !.kernel1Active = FALSE,
                           !.selection = "Spent", !.claimLeaf = FALSE,
                           !.selectPin = FALSE, !.fundingGrant = FALSE,
                           !.stoppedAtClaim = f.stopped, !.badAdmission = (@ \/ ~FAdmission)]
FRefuse == /\ f.phase = "Remote"
           /\ ((f.selection = "Locked" /\ ~f.admitted)
               \/ (f.selection = "Observed" /\ (f.stopped \/ ~f.contextCredit \/ ~f.domainCredit)))
           /\ f' = [f EXCEPT !.selection = "Refused", !.claimLeaf = FALSE,
                    !.selectPin = FALSE, !.reader = FALSE, !.fundingGrant = FALSE]
FClassPublish == /\ f.phase = "Remote" /\ ~f.claimLeaf /\ f.classEpoch = 0
                 /\ f' = [f EXCEPT !.classEpoch = 1]
FContextDebit == /\ f.phase = "Remote" /\ ~f.claimLeaf /\ f.contextCredit
                 /\ f' = [f EXCEPT !.contextCredit = FALSE]
FDomainDebit == /\ f.phase = "Remote" /\ ~f.claimLeaf /\ f.domainCredit
                /\ f' = [f EXCEPT !.domainCredit = FALSE]
FQueueRepublish == /\ f.phase = "Remote" /\ ~f.claimLeaf /\ ~f.reader /\ f.queueGeneration = 1
                  /\ f' = [f EXCEPT !.queueGeneration = 2]
FStopBeforeClaim == /\ f.phase = "Remote" /\ ~f.claimLeaf /\ ~f.census
                    /\ f' = [f EXCEPT !.census = TRUE, !.stopped = TRUE]
FPostClaimDonation == /\ f.phase \in {"IncomingAwait", "PeerCurrent"} /\ f.borrowedContext = 0
                      /\ f' = [f EXCEPT !.borrowedContext = 1]
FEffect == /\ f.phase \in {"IncomingEffects", "LeavingEffects", "SourceEffects"}
           /\ f.effects < 5
           /\ f' = [f EXCEPT !.effects = @ + 1,
                !.badEffect = (@ \/ ~f.sourceBank
                    \/ (f.phase # "SourceEffects" /\ ~f.sourceActive)
                    \/ (f.phase = "SourceEffects" /\ (~f.kernel0Active \/ f.sourceActive))
                    \/ (f.phase = "IncomingEffects" /\ (~f.peerPin \/ ~f.peerActive \/ f.prepared # 2))
                    \/ (f.phase = "LeavingEffects" /\ (~f.peerPin \/ f.current # 2 \/ ~f.kernel1Active)))]
FReady == /\ f.phase \in {"IncomingEffects", "LeavingEffects", "SourceEffects"}
          /\ f.effects = 5
          /\ f' = [f EXCEPT !.phase = IF @ = "IncomingEffects" THEN "IncomingReady"
                      ELSE IF @ = "LeavingEffects" THEN "LeavingReady" ELSE "SourceReady"]
FIncoming == /\ f.phase = "IncomingReady"
             /\ f' = [f EXCEPT !.phase = "IncomingAwait", !.current = 2, !.uidCurrent = 2,
                      !.prepared = 3, !.outgoing = 0, !.uidOutgoing = 0]
FStop == /\ f.phase \in {"IncomingAwait", "PeerCurrent"} /\ ~f.census
         /\ f' = [f EXCEPT !.census = TRUE, !.stopped = TRUE]
FIncomingAck == /\ f.phase = "IncomingAwait"
                /\ f' = [f EXCEPT !.phase = "PeerCurrent", !.outgoing = 3, !.uidOutgoing = 3]
FLeave == /\ f.phase = "PeerCurrent" /\ f.stopped /\ f.census /\ f.peerActive
          /\ f' = [f EXCEPT !.phase = "LeavingEffects", !.effects = 0,
                   !.peerActive = FALSE, !.peerCharged = @ + 1, !.kernel1Active = TRUE,
                   !.chargedContext = f.originalContext,
                   !.badCharge = (@ \/ f.peerCharged # 0 \/ f.originalContext # 2)]
FLeaving == /\ f.phase = "LeavingReady"
            /\ f' = [f EXCEPT !.phase = "LeavingAwait", !.current = 0, !.uidCurrent = 0,
                     !.outgoing = 2, !.uidOutgoing = 2]
FLeavingAck == /\ f.phase = "LeavingAwait"
               /\ f' = [f EXCEPT !.phase = "KernelCurrent", !.outgoing = 3,
                        !.uidOutgoing = 3, !.peerPin = FALSE]
FDetach == /\ f.phase = "KernelCurrent" /\ f.census /\ f.stopped
           /\ f.current # 2 /\ f.outgoing # 2 /\ f.uidCurrent # 2 /\ f.uidOutgoing # 2 /\ ~f.peerPin
           /\ f' = [f EXCEPT !.phase = "Detached", !.detached = TRUE,
                    !.borrowedContext = 0,
                    !.badCensus = (@ \/ f.current = 2 \/ f.outgoing = 2 \/ f.peerPin)]
FDetachUnclaimed == /\ f.phase = "Remote" /\ f.census /\ f.stopped
                    /\ f.selection \in {"Empty", "Refused"} /\ ~f.selectPin /\ ~f.reader /\ ~f.fundingGrant
                    /\ ~f.claimLeaf /\ ~f.peerPin /\ ~f.peerActive /\ f.current = 0
                    /\ f' = [f EXCEPT !.phase = "Detached", !.detached = TRUE,
                         !.badCensus = (@ \/ f.selectPin \/ f.reader \/ f.fundingGrant \/ f.peerPin)]
FKernelAck == /\ f.phase = "Detached" /\ f.detached
              /\ f' = [f EXCEPT !.phase = "ExecImage", !.kernelAck = TRUE,
                       !.badDetach = (@ \/ ~f.detached \/ f.current # 0)]
FRuntimeAck == /\ f.detached /\ ~f.runtimeAck
               /\ f' = [f EXCEPT !.runtimeAck = TRUE]
FExec == /\ f.phase = "ExecImage" /\ f.kernelAck /\ f.census /\ f.sourceBank /\ f.sourceActive
         /\ f' = [f EXCEPT !.phase = "ExecPublished", !.sourceMM = 9, !.published = TRUE]
FFinalize == /\ f.phase = "ExecPublished" /\ f.published /\ f.detached /\ ~f.peerPin
             /\ f' = [f EXCEPT !.phase = "Finalized", !.census = FALSE, !.oldRetired = TRUE]
FPeerReset == /\ f.phase = "Finalized" /\ ~f.census /\ f.kernelAck /\ f.runtimeAck /\ ~f.peerPin
              /\ f' = [f EXCEPT !.phase = "PeerReset", !.peerReset = TRUE,
                    !.badReuse = (@ \/ f.census \/ ~f.kernelAck \/ f.peerPin)]
FSourceStop == /\ f.phase = "PeerReset" /\ f.sourceActive
               /\ f' = [f EXCEPT !.phase = "SourceEffects", !.effects = 0,
                        !.sourceActive = FALSE, !.kernel0Active = TRUE, !.sourcePublished = TRUE,
                        !.sourceCharged = @ + 1, !.badCharge = (@ \/ f.sourceCharged # 0)]
FSourcePublish == /\ f.phase = "SourceReady" /\ ~f.sourceActive /\ f.kernel0Active
                  /\ f' = [f EXCEPT !.phase = "SourceAwait"]
FSourceAck == /\ f.phase = "SourceAwait"
              /\ f' = [f EXCEPT !.phase = "SourceKernel", !.sourceBank = FALSE]
FSourceRuntimeAck == /\ f.sourcePublished /\ ~f.sourceRuntimeAck
                     /\ f' = [f EXCEPT !.sourceRuntimeAck = TRUE]
FSourceKernelAck == /\ f.phase = "SourceKernel" /\ f.sourcePublished /\ ~f.sourceKernelAck
                    /\ f.kernel0Active /\ ~f.sourceBank
                    /\ f' = [f EXCEPT !.sourceKernelAck = TRUE]
FSourceReset == /\ f.phase = "SourceKernel" /\ ~f.sourceActive /\ ~f.sourceBank
                /\ f.sourceRuntimeAck /\ f.sourceKernelAck
                /\ f' = [f EXCEPT !.phase = "Done", !.sourceReset = TRUE, !.slotUID = 0,
                         !.badReuse = (@ \/ f.sourceActive \/ f.sourceBank)]
\* Rebirth is an independently reserved ORIGINAL constructor ticket, not a
\* generation lookup. Both kernel banks/intervals survive this entire suffix.
\* birthNonce abstracts the original constructor/UID incarnation, NOT a scalar
\* generation field added to the runtime ABI. Old runtime ACK payloads carry the
\* old task UID; the NEW Claimed row checks that original complete payload.
FBirthReserve == /\ f.phase = "Done" /\ f.sourceReset /\ f.peerReset /\ f.oldRetired
                 /\ ~f.census /\ ~f.sourceBank /\ ~f.peerPin /\ ~f.selectPin /\ ~f.fundingGrant
                 /\ f.kernel0Active /\ f.kernel1Active /\ f.slotUID = 0
                 /\ f' = [f EXCEPT !.phase = "BirthReserved", !.birthTicket = TRUE, !.birthNonce = 2]
\* The hidden original Ticket does not excuse an unpublished Identity writer.
\* Affinity is initialized only after exact UID publication and before Blocked ACK.
FBirthIdentity == /\ f.phase = "BirthReserved" /\ f.birthTicket /\ f.birthIdentity = 0
                  /\ f' = [f EXCEPT !.birthIdentity = 3]
FBirthAffinity == /\ f.phase = "BirthReserved" /\ f.birthTicket /\ f.birthAffinity = 0
                  /\ f.birthIdentity = 3
                  /\ f' = [f EXCEPT !.birthAffinity = 3]
FBirthPublish == /\ f.phase = "BirthReserved" /\ f.birthTicket /\ f.birthNonce = 2
                 /\ f.birthIdentity = 3 /\ f.birthAffinity = 3
                 /\ f' = [f EXCEPT !.phase = "Reborn", !.birthTicket = FALSE,
                          !.slotUID = 3, !.born = TRUE, !.sourceMM = 3]
FRebornStop == /\ f.phase = "Reborn" /\ f.slotUID = 3 /\ f.birthNonce = 2
               /\ f.kernel0Active /\ f.kernel1Active
               /\ f' = [f EXCEPT !.phase = "NewTerminal", !.newProducer = f.birthNonce]
FNewExternalAck == /\ f.phase = "NewTerminal" /\ f.newProducer = f.birthNonce /\ ~f.newExternal
                   /\ f' = [f EXCEPT !.newExternal = TRUE, !.ackNonce = f.birthNonce]
FOldExternalAck == /\ f.phase = "NewTerminal" /\ ~f.oldAckTried /\ ~f.newExternal
                   /\ f' = [f EXCEPT !.oldAckTried = TRUE,
                        !.newExternal = IF f.birthNonce = 1 THEN TRUE ELSE @,
                        !.ackNonce = IF f.birthNonce = 1 THEN 1 ELSE @]
FNewKernelAck == /\ f.phase = "NewTerminal" /\ f.newProducer = f.birthNonce /\ ~f.newKernel
                 /\ f' = [f EXCEPT !.newKernel = TRUE]
FRebornReset == /\ f.phase = "NewTerminal" /\ f.newExternal /\ f.newKernel
               /\ f.kernel0Active /\ f.kernel1Active /\ f.slotUID = 3
               /\ f' = [f EXCEPT !.phase = "Complete", !.slotUID = 0]
ForeignNext == FRemote \/ FSelect \/ FObserve \/ FFund \/ FJoin \/ FClaim \/ FRefuse
    \/ FClassPublish \/ FContextDebit \/ FDomainDebit \/ FQueueRepublish \/ FStopBeforeClaim
    \/ FPostClaimDonation \/ FEffect \/ FReady \/ FIncoming \/ FStop \/ FIncomingAck
    \/ FLeave \/ FLeaving \/ FLeavingAck \/ FDetach \/ FDetachUnclaimed \/ FKernelAck \/ FRuntimeAck \/ FExec \/ FFinalize
    \/ FPeerReset \/ FSourceStop \/ FSourcePublish \/ FSourceAck \/ FSourceRuntimeAck \/ FSourceKernelAck \/ FSourceReset
    \/ FBirthReserve \/ FBirthIdentity \/ FBirthAffinity \/ FBirthPublish \/ FRebornStop \/ FNewExternalAck \/ FOldExternalAck \/ FNewKernelAck \/ FRebornReset
    \/ UNCHANGED f
ForeignOriginalBanks == /\ f.current = f.uidCurrent /\ f.outgoing = f.uidOutgoing
    /\ (f.phase \in {"IncomingEffects","IncomingReady"} => (f.current = 0 /\ f.prepared = 2 /\ f.peerPin))
    /\ (f.phase = "LeavingAwait" => (f.outgoing = 2 /\ f.peerPin))
ForeignOriginalEffects == /\ ~f.badEffect
    /\ (f.phase \in {"IncomingReady","IncomingAwait","LeavingReady","LeavingAwait","SourceReady","SourceAwait"} => f.effects = 5)
ForeignCensusDrain == /\ ~f.badCensus /\ ~f.badDetach
    /\ (f.detached => (f.current # 2 /\ f.outgoing # 2 /\ ~f.peerPin))
    /\ (f.kernelAck => f.detached)
    /\ (f.published => (f.detached /\ f.kernelAck /\ f.sourceMM = (IF f.born THEN 3 ELSE 9) /\ ~f.peerPin))
ForeignSealedReuse == /\ ~f.badReuse
    /\ (f.peerReset => (~f.census /\ f.kernelAck /\ f.runtimeAck /\ ~f.peerPin /\ f.oldRetired))
    /\ (f.sourceReset => (~f.sourceActive /\ ~f.sourceBank /\ f.sourceRuntimeAck /\ f.sourceKernelAck))
    /\ (f.sourceKernelAck => f.sourcePublished /\ ~f.sourceBank /\ f.kernel0Active)
ForeignOriginalFunding == ~f.badCharge /\ f.peerCharged \in 0..1 /\ f.sourceCharged \in 0..1
    /\ (f.peerActive => f.originalContext = 2)
    /\ (f.peerCharged = 1 => f.chargedContext = 2)
ForeignSelectionJoin == /\ ~f.badAdmission
    /\ f.selection \in {"Empty","Reading","Observed","Funded","Locked","Refused","Spent"}
    /\ (f.selection = "Spent" => f.classEpoch = f.observedClass
          /\ f.queueGeneration = f.observedGeneration /\ f.contextCredit /\ f.domainCredit /\ ~f.stoppedAtClaim)
    /\ (f.selection \in {"Reading","Observed","Funded","Locked"} => f.selectPin)
    /\ (f.selection = "Reading" => f.reader)
    /\ (f.selection \in {"Funded","Locked"} => f.fundingGrant)
    /\ (f.claimLeaf = (f.selection = "Locked"))
    /\ (f.selection = "Locked" /\ f.admitted => FAdmission)
ForeignBorrowerDrain == f.detached => (~f.selectPin /\ ~f.reader /\ ~f.fundingGrant /\ ~f.claimLeaf)
ForeignConstructorPublication == /\ (f.birthAffinity # 0 => f.birthIdentity = 3)
    /\ (f.born => f.birthIdentity = 3 /\ f.birthAffinity = 3)
ForeignNewIncarnation == /\ (f.birthTicket => f.slotUID = 0)
    /\ (f.phase = "BirthReserved" => f.birthTicket)
    /\ (f.born => f.sourceReset /\ f.peerReset /\ f.oldRetired /\ f.birthNonce = 2
                  /\ f.sourceMM = 3 /\ f.kernel0Active /\ f.kernel1Active)
    /\ (f.newExternal => f.ackNonce = f.birthNonce /\ f.newProducer = f.birthNonce)
    /\ (f.newKernel => f.newProducer = f.birthNonce)
    /\ (f.phase = "Complete" => f.newExternal /\ f.newKernel /\ f.slotUID = 0)
Init == /\ ComponentInit /\ JointInit /\ CycleInit /\ ForeignInit
        /\ proofMode \in {"Components", "Cycle", "Foreign"}
Next == \/ (proofMode = "Components" /\ UNCHANGED <<c,f,proofMode>>
             /\ ((ComponentNext /\ UNCHANGED jointVars)
                 \/ (JointNext /\ UNCHANGED componentVars)))
        \/ (proofMode = "Cycle" /\ CycleNext
             /\ UNCHANGED <<componentVars,jointVars,f,proofMode>>)
        \/ (proofMode = "Foreign" /\ ForeignNext
             /\ UNCHANGED <<componentVars,jointVars,c,proofMode>>)
=============================================================================
