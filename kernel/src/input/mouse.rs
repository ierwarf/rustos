use core::sync::atomic::{AtomicBool, Ordering};

static MOUSE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_available(available: bool) {
    MOUSE_ACTIVE.store(available, Ordering::Release);
    if available {
        let _ = crate::gui::show_mouse_cursor();
    }
}

pub fn is_available() -> bool {
    MOUSE_ACTIVE.load(Ordering::Acquire)
}

pub fn on_relative_motion(dx: i8, dy: i8) -> bool {
    if !is_available() || (dx == 0 && dy == 0) {
        return false;
    }

    crate::ui_service::push_pointer_motion(dx as i16, dy as i16);
    crate::gui::move_mouse_cursor_relative(dx as i16, dy as i16)
}

pub fn on_left_button_changed(pressed: bool) -> bool {
    if !is_available() {
        return false;
    }

    crate::ui_service::push_pointer_button_left(pressed);
    crate::gui::set_mouse_left_button(pressed)
}

#[cfg(test)]
mod tests {
    use super::{is_available, on_left_button_changed, on_relative_motion, set_available};
    use crate::gui;

    #[test]
    fn activation_is_tracked() {
        gui::reset_mouse_state();
        set_available(false);
        assert!(!is_available());
        set_available(true);
        assert!(is_available());
        assert!(gui::mouse_visible());
        assert_eq!(gui::mouse_show_count(), 1);
    }

    #[test]
    fn zero_motion_is_ignored() {
        gui::reset_mouse_state();
        set_available(true);
        assert!(!on_relative_motion(0, 0));
    }

    #[test]
    fn usb_y_axis_matches_screen_space() {
        gui::reset_mouse_state();
        set_available(true);
        assert!(on_relative_motion(4, -3));
        assert_eq!(gui::last_mouse_move(), (4, -3));
    }

    #[test]
    fn left_button_changes_are_forwarded() {
        gui::reset_mouse_state();
        set_available(true);
        assert!(on_left_button_changed(true));
        assert!(gui::last_mouse_left_button());
        assert!(on_left_button_changed(false));
        assert!(!gui::last_mouse_left_button());
    }
}
