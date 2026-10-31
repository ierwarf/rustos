//! Rootd bootstrap-only verification of cross-service positive operation
//! grants. Executed before rootd makes its endpoint externally visible.
//! This is a compatibility guard, not a capability minting operation.
use rustos_component_model::policy_ops::SlowBrokerOperation as Op;
use rustos_component_model::service_catalog::{self,ROOT_SERVICE_ID,SERVICES};
use super::service_grants;

pub(super) fn preflight()->Result<(),()> {
    if service_catalog::validate().is_err(){return Err(())}
    // Every catalog component has exactly one immutable positive role and
    // there are no unowned grants except rootd's supervisor authority.
    for service in SERVICES {
        if service_grants::GRANTS.iter()
            .filter(|g|g.service==service.id).count()!=1 {return Err(())}
        if !service_grants::permits(service.id,Op::ResolveServiceRoute){
            return Err(())
        }
    }
    for grant in service_grants::GRANTS {
        if grant.service!=ROOT_SERVICE_ID &&
            !SERVICES.iter().any(|s|s.id==grant.service){
            return Err(())
        }
    }
    // Explicitly restrict high-risk operations to their natural holders.
    let exclusive:&[(Op,&[u64])]=&[
        (Op::ChargeClientSession,&[11]),
        (Op::ReclaimServiceLease,&[15]),
        (Op::ConfigureNetwork,&[3]),
        (Op::ModifyFilesystemPolicy,&[2]),
        (Op::UpgradeComponent,&[6,15]),
        (Op::RollbackComponent,&[15]),
    ];
    for (op,holders) in exclusive {
        for service in SERVICES {
            if service_grants::permits(service.id,*op)
                != holders.contains(&service.id) {
                return Err(())
            }
        }
    }
    // A negative Job ceiling is a *second* authorization gate. This ensures
    // an op bit collision cannot be induced by future enum reordering.
    let all=Op::ALL;
    for i in 0..all.len() {
        if all[i] as u16 >= 64 || all[..i].iter().any(|o|*o as u16==all[i] as u16){
            return Err(())
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bootstrap_roster_has_no_implicit_charging_authority() {
        assert_eq!(preflight(),Ok(()));
        assert!(service_grants::permits(11,Op::ChargeClientSession));
        assert!(!service_grants::permits(3,Op::ChargeClientSession));
        assert!(!service_grants::permits(2,Op::ChargeClientSession));
    }
}
