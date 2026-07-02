// RING3-MIGRATION-REFERENCE START: devmgrd/inputd should own input device
// readiness policy. Ring0 keeps native input device bridge substrate.
use crate::input::event_queue;

pub fn has_pending_events() -> bool {
    event_queue::has_pending_input_events()
}
// RING3-MIGRATION-REFERENCE END: devmgrd/inputd-owned input device policy.
