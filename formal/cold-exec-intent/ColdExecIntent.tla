------------------------- MODULE ColdExecIntent -------------------------
EXTENDS Naturals, TLC
VARIABLES uid, bound, gen, phase, self, gate, writer, oldGrant, reserved,
          cancelled, staged, published, finalized, pendingExit, exiting,
          staleEffect, badReset, prematureOpen
vars == <<uid,bound,gen,phase,self,gate,writer,oldGrant,reserved,cancelled,
          staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Init == /\ uid = 1 /\ bound = 1 /\ gen = 0 /\ phase = "Idle"
        /\ self \in BOOLEAN /\ oldGrant \in BOOLEAN
        /\ gate = FALSE /\ writer = FALSE /\ reserved = FALSE /\ cancelled = FALSE
        /\ staged = FALSE /\ published = FALSE /\ finalized = FALSE
        /\ pendingExit = FALSE /\ exiting = FALSE /\ staleEffect = FALSE
        /\ badReset = FALSE /\ prematureOpen = FALSE
Reserve == /\ phase = "Idle" /\ ~reserved /\ ~exiting /\ gen < 2
           /\ reserved' = TRUE /\ gen' = gen + 1
           /\ UNCHANGED <<uid,bound,phase,self,gate,writer,oldGrant,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Begin == /\ phase = "Idle" /\ reserved /\ ~exiting
         /\ phase' = "Sealed" /\ bound' = uid /\ gate' = ~self
         /\ UNCHANGED <<uid,gen,self,writer,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
ReleaseGrant == /\ oldGrant /\ oldGrant' = FALSE
                /\ UNCHANGED <<uid,bound,gen,phase,self,gate,writer,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Image == /\ phase = "Sealed" /\ (self \/ ~oldGrant) /\ reserved
         /\ phase' = "Image" /\ gate' = TRUE /\ writer' = TRUE
         /\ UNCHANGED <<uid,bound,gen,self,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
AbortImage == /\ phase = "Image" /\ ~staged /\ ~published
              /\ phase' = "Sealed" /\ writer' = FALSE /\ gate' = ~self
              /\ UNCHANGED <<uid,bound,gen,self,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Stage == /\ phase = "Image" /\ writer /\ ~oldGrant /\ ~staged
         /\ staged' = TRUE
         /\ UNCHANGED <<uid,bound,gen,phase,self,gate,writer,oldGrant,reserved,cancelled,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Publish == /\ phase = "Image" /\ staged /\ writer
           /\ phase' = "Published" /\ published' = TRUE
           /\ UNCHANGED <<uid,bound,gen,self,gate,writer,oldGrant,reserved,cancelled,staged,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Finalize == /\ phase = "Published" /\ published /\ reserved
            /\ finalized' = TRUE /\ reserved' = FALSE /\ exiting' = (exiting \/ pendingExit)
            /\ UNCHANGED <<uid,bound,gen,phase,self,gate,writer,oldGrant,cancelled,staged,published,pendingExit,staleEffect,badReset,prematureOpen>>
ReleaseWriter == /\ phase = "Published" /\ finalized /\ writer
                 /\ writer' = FALSE
                 /\ UNCHANGED <<uid,bound,gen,phase,self,gate,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Complete == /\ phase = "Published" /\ finalized /\ ~writer
            /\ phase' = "Idle" /\ gate' = exiting /\ staged' = FALSE
            /\ published' = FALSE /\ finalized' = FALSE
            /\ prematureOpen' = (prematureOpen \/ (~finalized) \/ writer)
            /\ UNCHANGED <<uid,bound,gen,self,writer,oldGrant,reserved,cancelled,pendingExit,exiting,staleEffect,badReset>>
CloseCancel == /\ phase = "Sealed" /\ reserved
               /\ phase' = "Cancelling" /\ gate' = TRUE
               /\ UNCHANGED <<uid,bound,gen,self,writer,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
CancelProcess == /\ phase = "Cancelling" /\ reserved
                 /\ reserved' = FALSE /\ cancelled' = TRUE /\ exiting' = (exiting \/ pendingExit)
                 /\ UNCHANGED <<uid,bound,gen,phase,self,gate,writer,oldGrant,staged,published,finalized,pendingExit,staleEffect,badReset,prematureOpen>>
CancelIntent(expected) == /\ phase = "Cancelling" /\ cancelled
                          /\ IF expected = gen
                             THEN /\ phase' = "Idle" /\ gate' = exiting /\ cancelled' = FALSE
                                  /\ staleEffect' = (staleEffect \/ expected # gen)
                                  /\ UNCHANGED <<uid,bound,gen,self,writer,oldGrant,reserved,staged,published,finalized,pendingExit,exiting,badReset,prematureOpen>>
                             ELSE UNCHANGED vars
StaleCancel == /\ gen = 2 /\ CancelIntent(1)
PendingExit == /\ reserved /\ ~pendingExit /\ pendingExit' = TRUE
               /\ UNCHANGED <<uid,bound,gen,phase,self,gate,writer,oldGrant,reserved,cancelled,staged,published,finalized,exiting,staleEffect,badReset,prematureOpen>>
Poison == /\ phase \in {"Sealed","Cancelling","Image","Published"}
          /\ phase' = "Poisoned" /\ gate' = TRUE
          /\ UNCHANGED <<uid,bound,gen,self,writer,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>>
Reset == /\ phase = "Idle" /\ ~reserved /\ ~oldGrant /\ uid = 1
         /\ uid' = 2 /\ bound' = 2
         /\ badReset' = (badReset \/ phase # "Idle" \/ reserved \/ oldGrant)
         /\ UNCHANGED <<gen,phase,self,gate,writer,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,prematureOpen>>
Next == Reserve \/ Begin \/ ReleaseGrant \/ Image \/ AbortImage \/ Stage \/ Publish \/ Finalize \/ ReleaseWriter \/ Complete \/ CloseCancel \/ CancelProcess \/ CancelIntent(gen) \/ StaleCancel \/ PendingExit \/ Poison \/ Reset \/ UNCHANGED vars
TypeOK == /\ uid \in {1,2} /\ bound \in {1,2} /\ gen \in 0..2
          /\ phase \in {"Idle","Sealed","Cancelling","Image","Published","Poisoned"}
          /\ <<self,gate,writer,oldGrant,reserved,cancelled,staged,published,finalized,pendingExit,exiting,staleEffect,badReset,prematureOpen>> \in BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN
StaleCommandsHaveNoEffect == ~staleEffect
GateClosedThroughImage == phase \in {"Cancelling","Image","Published","Poisoned"} => gate
BindingPreserved == phase # "Idle" => uid = bound
NoResetWithOwner == ~badReset
ReceiptBeforeReopen == ~prematureOpen
TerminalNeverReopens == exiting => gate
=============================================================================
