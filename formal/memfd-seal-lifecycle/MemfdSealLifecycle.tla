---------------------- MODULE MemfdSealLifecycle ----------------------
EXTENDS Naturals

(***************************************************************************
Owner: kernel/ps memfd object and mapping holds.
Linearization point: the MemfdState mutex. Seal installation, mapping
admission, resize, and mapping release are one serialized object lifecycle.
***************************************************************************)

CONSTANT MaxMappings, MaxSize
VARIABLES size, mappings, writableMappings, sealWrite, sealGrow, sealShrink,
          sealSeals
vars == <<size, mappings, writableMappings, sealWrite, sealGrow, sealShrink,
          sealSeals>>

Init ==
    /\ size = 1 /\ mappings = 0 /\ writableMappings = 0
    /\ sealWrite = FALSE /\ sealGrow = FALSE /\ sealShrink = FALSE
    /\ sealSeals = FALSE

Map(writable) ==
    /\ mappings < MaxMappings
    /\ (~writable \/ ~sealWrite)
    /\ mappings' = mappings + 1
    /\ writableMappings' = writableMappings + IF writable THEN 1 ELSE 0
    /\ UNCHANGED <<size, sealWrite, sealGrow, sealShrink, sealSeals>>

Unmap(writable) ==
    /\ mappings > 0
    /\ IF writable THEN writableMappings > 0 ELSE mappings > writableMappings
    /\ mappings' = mappings - 1
    /\ writableMappings' = writableMappings - IF writable THEN 1 ELSE 0
    /\ UNCHANGED <<size, sealWrite, sealGrow, sealShrink, sealSeals>>

AddSeals(addWrite, addGrow, addShrink, addSeal) ==
    /\ ~sealSeals
    /\ addWrite \in BOOLEAN /\ addGrow \in BOOLEAN
    /\ addShrink \in BOOLEAN /\ addSeal \in BOOLEAN
    /\ addWrite \/ addGrow \/ addShrink \/ addSeal
    /\ (~addWrite \/ writableMappings = 0)
    /\ sealWrite' = (sealWrite \/ addWrite)
    /\ sealGrow' = (sealGrow \/ addGrow)
    /\ sealShrink' = (sealShrink \/ addShrink)
    /\ sealSeals' = (sealSeals \/ addSeal)
    /\ UNCHANGED <<size, mappings, writableMappings>>

Grow ==
    /\ size < MaxSize /\ ~sealGrow
    /\ size' = size + 1
    /\ UNCHANGED <<mappings, writableMappings, sealWrite, sealGrow, sealShrink,
                    sealSeals>>

Shrink ==
    /\ size > 0 /\ ~sealShrink /\ mappings = 0
    /\ size' = size - 1
    /\ UNCHANGED <<mappings, writableMappings, sealWrite, sealGrow, sealShrink,
                    sealSeals>>

Write(newSize) ==
    /\ newSize \in size..MaxSize /\ ~sealWrite
    /\ (newSize = size \/ ~sealGrow)
    /\ size' = newSize
    /\ UNCHANGED <<mappings, writableMappings, sealWrite, sealGrow, sealShrink,
                    sealSeals>>

Next == Map(TRUE) \/ Map(FALSE) \/ Unmap(TRUE) \/ Unmap(FALSE)
        \/ \E aw \in BOOLEAN, ag \in BOOLEAN, ash \in BOOLEAN, aseal \in BOOLEAN:
               AddSeals(aw, ag, ash, aseal)
        \/ Grow \/ Shrink \/ \E newSize \in 0..MaxSize: Write(newSize)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ size \in 0..MaxSize /\ mappings \in 0..MaxMappings
    /\ writableMappings \in 0..MaxMappings
    /\ sealWrite \in BOOLEAN /\ sealGrow \in BOOLEAN /\ sealShrink \in BOOLEAN
    /\ sealSeals \in BOOLEAN
WritableMappingsAreMappings == writableMappings <= mappings
WriteSealHasNoWritableMapping == sealWrite => writableMappings = 0
CountsRemainBounded == mappings <= MaxMappings

=============================================================================
