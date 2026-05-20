use crate::input::event_queue;

pub fn has_pending_events() -> bool {
    event_queue::has_pending_input_events()
}
