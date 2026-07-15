-------------------------- MODULE DmaIommuIsolation --------------------------
EXTENDS Naturals, FiniteSets, TLC

(*******************************************************************************
Owner: L0 hostd for DVM devices and kernel io-manager for boot-storage DMA.
Linearization point: ActivateDomain after hardware remapping and invalidation.
No identity/no-IOMMU state may be active in a commercial topology.
*******************************************************************************)

CONSTANTS Devices, Domains, Pages, MaxMappings, MaxEpoch

NoDomain == "none"
Allowed(dom) == IF dom = "boot" THEN {"p0"} ELSE {"p1"}
VARIABLES owner, active, mappings, rejected, invalidationEpoch
vars == <<owner, active, mappings, rejected, invalidationEpoch>>

Init ==
    /\ owner = [d \in Devices |-> NoDomain]
    /\ active = {}
    /\ mappings = {}
    /\ rejected = FALSE
    /\ invalidationEpoch = 0

Assign(d, dom) ==
    /\ owner[d] = NoDomain
    /\ owner' = [owner EXCEPT ![d] = dom]
    /\ UNCHANGED <<active, mappings, rejected, invalidationEpoch>>

ActivateDomain(dom) ==
    /\ dom \notin active
    /\ \E d \in Devices: owner[d] = dom
    /\ active' = active \cup {dom}
    /\ invalidationEpoch' = (invalidationEpoch + 1) % (MaxEpoch + 1)
    /\ UNCHANGED <<owner, mappings, rejected>>

Map(d, dom, page) ==
    /\ owner[d] = dom
    /\ dom \in active
    /\ page \in Allowed(dom)
    /\ Cardinality(mappings) < MaxMappings
    /\ mappings' = mappings \cup {<<d, dom, page>>}
    /\ UNCHANGED <<owner, active, rejected, invalidationEpoch>>

RejectMap ==
    /\ rejected' = TRUE
    /\ UNCHANGED <<owner, active, mappings, invalidationEpoch>>

Revoke(dom) ==
    /\ dom \in active
    /\ active' = active \ {dom}
    /\ mappings' = {m \in mappings: m[2] # dom}
    /\ owner' = [d \in Devices |-> IF owner[d] = dom THEN NoDomain ELSE owner[d]]
    /\ invalidationEpoch' = (invalidationEpoch + 1) % (MaxEpoch + 1)
    /\ UNCHANGED rejected

Next ==
    \/ \E d \in Devices, dom \in Domains: Assign(d, dom)
    \/ \E dom \in Domains: ActivateDomain(dom)
    \/ \E d \in Devices, dom \in Domains, page \in Pages: Map(d, dom, page)
    \/ RejectMap
    \/ \E dom \in Domains: Revoke(dom)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ owner \in [Devices -> Domains \cup {NoDomain}]
    /\ active \in SUBSET Domains
    /\ mappings \in SUBSET (Devices \X Domains \X Pages)
    /\ rejected \in BOOLEAN
    /\ invalidationEpoch \in 0..MaxEpoch

MappingsHaveExactOwner == \A m \in mappings: owner[m[1]] = m[2]
MappingsStayInAperture == \A m \in mappings: m[3] \in Allowed(m[2])
RevokedDomainsHaveNoMappings == \A m \in mappings: m[2] \in active
MappingsAreBounded == Cardinality(mappings) <= MaxMappings

=============================================================================
