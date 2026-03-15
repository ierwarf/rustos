use crate::keyboard::KeyboardEvent;

pub(crate) fn dispatch_keyboard_event(event: KeyboardEvent) {
    crate::ui_service::push_keyboard_event(event);
    let session = crate::session::focused_console_session();
    crate::tty::on_key_event_for_session(session, event);
}
