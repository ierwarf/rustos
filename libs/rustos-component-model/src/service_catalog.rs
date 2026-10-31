//! The rootd/initd shared declarative service roster.
//!
//! Names and paths select only a policy record, never kernel authority.
//! The actual sender has to be proven against the current Job/lease graph
//! before a descriptor, route or service endpoint can be delivered.
//!
//! Service IDs are stable public ABI IDs; rootd preflight cross-checks this
//! catalog against the kernel's versioned service namespace at boot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Service {
    pub id: u64,
    pub parent_id: u64,
    pub exec: &'static str,
    pub critical: bool,
    pub bootstrap: bool,
}

pub const ROOT_SERVICE_ID: u64 = 10;
pub const INIT_SERVICE_ID: u64 = 15;

// Only one declaration of the supervisor-parent edges and executable names.
// Non-boot descendants must be parent-before-child in this slice.
pub const SERVICES: [Service; 12] = [
    Service { id: 1, parent_id: ROOT_SERVICE_ID, exec: "services/syscalld/syscalld.elf", critical: true, bootstrap: true },
    Service { id: 2, parent_id: ROOT_SERVICE_ID, exec: "services/vfsd/vfsd.elf", critical: true, bootstrap: true },
    Service { id: 6, parent_id: ROOT_SERVICE_ID, exec: "services/loaderd/loaderd.elf", critical: true, bootstrap: true },
    Service { id: 9, parent_id: ROOT_SERVICE_ID, exec: "services/procd/procd.elf", critical: true, bootstrap: true },
    Service { id: 12, parent_id: ROOT_SERVICE_ID, exec: "services/pagerd/pagerd.elf", critical: true, bootstrap: true },
    Service { id: 15, parent_id: ROOT_SERVICE_ID, exec: "services/initd/initd.elf", critical: true, bootstrap: true },
    Service { id: 3, parent_id: INIT_SERVICE_ID, exec: "services/netd/netd.elf", critical: false, bootstrap: false },
    Service { id: 4, parent_id: INIT_SERVICE_ID, exec: "services/devmgrd/devmgrd.elf", critical: true, bootstrap: false },
    Service { id: 8, parent_id: INIT_SERVICE_ID, exec: "services/inputd/inputd.elf", critical: true, bootstrap: false },
    Service { id: 7, parent_id: INIT_SERVICE_ID, exec: "services/storaged/storaged.elf", critical: false, bootstrap: false },
    Service { id: 11, parent_id: INIT_SERVICE_ID, exec: "services/runtimed/runtimed.elf", critical: true, bootstrap: false },
    Service { id: 14, parent_id: 11, exec: "services/uiserver/uiserver.elf", critical: true, bootstrap: false },
];

pub fn by_id(id: u64) -> Option<Service> {
    SERVICES.iter().copied().find(|spec| spec.id == id)
}

pub fn by_exec(exec: &str) -> Option<Service> {
    SERVICES.iter().copied().find(|spec| spec.exec == exec)
}

pub fn initd_owned_id_for_exec(exec: &str) -> Option<u64> {
    by_exec(exec).filter(|spec| spec.parent_id == INIT_SERVICE_ID).map(|spec| spec.id)
}

pub fn initd_owned_exec_for_id(id: u64) -> Option<&'static str> {
    by_id(id).filter(|spec| spec.parent_id == INIT_SERVICE_ID).map(|spec| spec.exec)
}

pub fn service_parent(id: u64) -> Option<u64> {
    by_id(id).map(|spec| spec.parent_id)
}

pub fn validate() -> Result<(), ()> {
    for (i, node) in SERVICES.iter().enumerate() {
        if node.id == 0 || node.parent_id == node.id
            || !node.exec.starts_with("services/")
            || !node.exec.ends_with(".elf")
            || node.exec.contains("..")
            || SERVICES[..i].iter().any(|prev| prev.id == node.id || prev.exec == node.exec)
        {
            return Err(());
        }
        if !node.bootstrap
            && !SERVICES[..i].iter().any(|prev| prev.id == node.parent_id)
        {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declarative_parent_edges_are_single_source_of_truth() {
        assert_eq!(validate(), Ok(()));
        assert_eq!(initd_owned_id_for_exec("services/netd/netd.elf"), Some(3));
        assert_eq!(initd_owned_exec_for_id(11), Some("services/runtimed/runtimed.elf"));
        assert_eq!(initd_owned_id_for_exec("services/uiserver/uiserver.elf"), None);
        assert_eq!(service_parent(14), Some(11));
        assert_eq!(SERVICES.len(), 12);
    }
}
