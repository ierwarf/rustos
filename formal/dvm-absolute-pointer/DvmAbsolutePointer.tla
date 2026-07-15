------------------------ MODULE DvmAbsolutePointer ------------------------
EXTENDS Integers, Naturals

(***************************************************************************
Models the production RDI3 absolute-pointer path:

  Linux evdev ABS_X/ABS_Y -> SYN_REPORT -> authenticated L0 frame ->
  fixed input ring -> ring0 decoder -> inputd -> uiserver cursor

Concrete owners:
  driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c
  libs/driver-domain-{protocol,host}/src/lib.rs
  kernel/io-manager/src/input/{dvm_frames,event_queue}.rs
  services/inputd/src/main.rs
  services/uiserver/src/{input_loop,app/input}.rs

Axis updates are staging only. A complete report publishes one bounded
position, and a report equal to the last published position is idempotent.
No layer may reinterpret an absolute position as relative motion. This is the
contract that rules out a stationary tablet making the cursor tremble around
the centre.
***************************************************************************)

CONSTANTS MaxX, MaxY, MaxPublishes

NoPosition == <<-1, -1>>
Positions == (0..MaxX) \X (0..MaxY)
Present(value) == IF value = NoPosition THEN 0 ELSE 1

VARIABLES rawX,
          rawY,
          seenX,
          seenY,
          lastPublished,
          dvmFrame,
          ringFrame,
          inputdEvent,
          uiEvent,
          initialCursor,
          cursor,
          lastDelivered,
          published,
          accepted,
          decoded,
          delivered,
          applied,
          uiMoves

vars == <<rawX, rawY, seenX, seenY, lastPublished, dvmFrame,
          ringFrame, inputdEvent, uiEvent, initialCursor, cursor,
          lastDelivered, published, accepted, decoded, delivered,
          applied, uiMoves>>

Init ==
    /\ rawX \in 0..MaxX
    /\ rawY \in 0..MaxY
    /\ seenX = FALSE
    /\ seenY = FALSE
    /\ lastPublished = NoPosition
    /\ dvmFrame = NoPosition
    /\ ringFrame = NoPosition
    /\ inputdEvent = NoPosition
    /\ uiEvent = NoPosition
    /\ initialCursor \in Positions
    /\ cursor = initialCursor
    /\ lastDelivered = NoPosition
    /\ published = 0
    /\ accepted = 0
    /\ decoded = 0
    /\ delivered = 0
    /\ applied = 0
    /\ uiMoves = 0

StageX(x) ==
    /\ x \in 0..MaxX
    /\ rawX' = x
    /\ seenX' = TRUE
    /\ UNCHANGED <<rawY, seenY, lastPublished, dvmFrame, ringFrame,
                  inputdEvent, uiEvent, initialCursor, cursor,
                  lastDelivered, published, accepted, decoded, delivered,
                  applied, uiMoves>>

StageY(y) ==
    /\ y \in 0..MaxY
    /\ rawY' = y
    /\ seenY' = TRUE
    /\ UNCHANGED <<rawX, seenX, lastPublished, dvmFrame, ringFrame,
                  inputdEvent, uiEvent, initialCursor, cursor,
                  lastDelivered, published, accepted, decoded, delivered,
                  applied, uiMoves>>

PublishReport ==
    /\ seenX /\ seenY
    /\ dvmFrame = NoPosition
    /\ published < MaxPublishes
    /\ <<rawX, rawY>> # lastPublished
    /\ dvmFrame' = <<rawX, rawY>>
    /\ lastPublished' = <<rawX, rawY>>
    /\ published' = published + 1
    /\ UNCHANGED <<rawX, rawY, seenX, seenY, ringFrame,
                  inputdEvent, uiEvent, initialCursor, cursor,
                  lastDelivered, accepted, decoded, delivered, applied,
                  uiMoves>>

(***************************************************************************
A complete duplicate SYN_REPORT is observable at evdev but produces no RDI3
frame and therefore cannot alter any downstream cursor/accounting state.
***************************************************************************)
DuplicateReport ==
    /\ seenX /\ seenY
    /\ <<rawX, rawY>> = lastPublished
    /\ UNCHANGED vars

IncompleteReport ==
    /\ ~(seenX /\ seenY)
    /\ UNCHANGED vars

RelayAtL0 ==
    /\ dvmFrame # NoPosition
    /\ ringFrame = NoPosition
    /\ ringFrame' = dvmFrame
    /\ dvmFrame' = NoPosition
    /\ accepted' = accepted + 1
    /\ UNCHANGED <<rawX, rawY, seenX, seenY, lastPublished,
                  inputdEvent, uiEvent, initialCursor, cursor,
                  lastDelivered, published, decoded, delivered, applied,
                  uiMoves>>

DecodeInKernel ==
    /\ ringFrame # NoPosition
    /\ inputdEvent = NoPosition
    /\ inputdEvent' = ringFrame
    /\ ringFrame' = NoPosition
    /\ decoded' = decoded + 1
    /\ UNCHANGED <<rawX, rawY, seenX, seenY, lastPublished,
                  dvmFrame, uiEvent, initialCursor, cursor,
                  lastDelivered, published, accepted, delivered, applied,
                  uiMoves>>

QueueInInputd ==
    /\ inputdEvent # NoPosition
    /\ uiEvent = NoPosition
    /\ uiEvent' = inputdEvent
    /\ inputdEvent' = NoPosition
    /\ delivered' = delivered + 1
    /\ UNCHANGED <<rawX, rawY, seenX, seenY, lastPublished,
                  dvmFrame, ringFrame, initialCursor, cursor,
                  lastDelivered, published, accepted, decoded, applied,
                  uiMoves>>

ApplyInUi ==
    /\ uiEvent # NoPosition
    /\ cursor' = uiEvent
    /\ lastDelivered' = uiEvent
    /\ uiEvent' = NoPosition
    /\ applied' = applied + 1
    /\ uiMoves' = IF uiEvent = cursor THEN uiMoves ELSE uiMoves + 1
    /\ UNCHANGED <<rawX, rawY, seenX, seenY, lastPublished,
                  dvmFrame, ringFrame, inputdEvent, initialCursor,
                  published, accepted, decoded, delivered>>

Next ==
    \/ \E x \in 0..MaxX : StageX(x)
    \/ \E y \in 0..MaxY : StageY(y)
    \/ PublishReport
    \/ DuplicateReport
    \/ IncompleteReport
    \/ RelayAtL0
    \/ DecodeInKernel
    \/ QueueInInputd
    \/ ApplyInUi

Spec ==
    Init /\ [][Next]_vars
    /\ WF_vars(RelayAtL0)
    /\ WF_vars(DecodeInKernel)
    /\ WF_vars(QueueInInputd)
    /\ WF_vars(ApplyInUi)

TypeOK ==
    /\ rawX \in 0..MaxX
    /\ rawY \in 0..MaxY
    /\ seenX \in BOOLEAN
    /\ seenY \in BOOLEAN
    /\ lastPublished \in Positions \cup {NoPosition}
    /\ dvmFrame \in Positions \cup {NoPosition}
    /\ ringFrame \in Positions \cup {NoPosition}
    /\ inputdEvent \in Positions \cup {NoPosition}
    /\ uiEvent \in Positions \cup {NoPosition}
    /\ initialCursor \in Positions
    /\ cursor \in Positions
    /\ lastDelivered \in Positions \cup {NoPosition}
    /\ published \in 0..MaxPublishes
    /\ accepted \in 0..MaxPublishes
    /\ decoded \in 0..MaxPublishes
    /\ delivered \in 0..MaxPublishes
    /\ applied \in 0..MaxPublishes
    /\ uiMoves \in 0..MaxPublishes

ExactSingleOwnerAccounting ==
    /\ published = accepted + Present(dvmFrame)
    /\ accepted = decoded + Present(ringFrame)
    /\ decoded = delivered + Present(inputdEvent)
    /\ delivered = applied + Present(uiEvent)

NoPartialOrDuplicatePublication ==
    /\ (published = 0) = (lastPublished = NoPosition)
    /\ (dvmFrame # NoPosition => dvmFrame = lastPublished)

NoPhantomMotion ==
    /\ uiMoves <= applied
    /\ applied <= published
    /\ (applied = 0 => cursor = initialCursor)
    /\ (applied > 0 => cursor = lastDelivered)

AbsoluteSemanticsRemainBounded ==
    /\ cursor \in Positions
    /\ dvmFrame \in Positions \cup {NoPosition}
    /\ ringFrame \in Positions \cup {NoPosition}
    /\ inputdEvent \in Positions \cup {NoPosition}
    /\ uiEvent \in Positions \cup {NoPosition}

EveryPublishedFrameEventuallyLeavesDvm ==
    dvmFrame # NoPosition ~> dvmFrame = NoPosition

EveryAcceptedFrameEventuallyLeavesRing ==
    ringFrame # NoPosition ~> ringFrame = NoPosition

EveryDecodedPositionEventuallyApplies ==
    inputdEvent # NoPosition ~> inputdEvent = NoPosition

EveryQueuedPositionEventuallyApplies ==
    uiEvent # NoPosition ~> uiEvent = NoPosition

=============================================================================
