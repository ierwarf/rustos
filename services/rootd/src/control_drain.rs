//! Bounded root control-endpoint draining.
//!
//! Rootd has lifecycle and restart owners beside its synchronous control
//! endpoint.  Drain a ready dependency burst without sleeping between calls,
//! but return after the shared budget so those other owners cannot starve.

use rustos_user_abi::performance::IPC_CONTROL_DRAIN_BUDGET;

const ROOTD_REQUEST_DRAIN_BUDGET: usize = IPC_CONTROL_DRAIN_BUDGET;

pub(super) fn drain_rootd_control_requests(mut serve_one: impl FnMut() -> bool) -> usize {
    let mut served = 0;
    while served < ROOTD_REQUEST_DRAIN_BUDGET && serve_one() {
        served += 1;
    }
    served
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_control_drain_services_a_bounded_ready_burst() {
        assert_eq!(ROOTD_REQUEST_DRAIN_BUDGET, IPC_CONTROL_DRAIN_BUDGET);
        assert_eq!(ROOTD_REQUEST_DRAIN_BUDGET, 32);
        let mut ready = ROOTD_REQUEST_DRAIN_BUDGET + 7;
        let served = drain_rootd_control_requests(|| {
            if ready == 0 {
                return false;
            }
            ready -= 1;
            true
        });
        assert_eq!(served, ROOTD_REQUEST_DRAIN_BUDGET);
        assert_eq!(ready, 7);

        let served = drain_rootd_control_requests(|| {
            if ready == 0 {
                return false;
            }
            ready -= 1;
            true
        });
        assert_eq!(served, 7);
        assert_eq!(ready, 0);
    }
}
