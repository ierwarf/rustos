pub(crate) type WindowId = usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowHitArea {
    Client,
    TitleBar,
    MinimizeButton,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowHit {
    pub(crate) window_id: WindowId,
    pub(crate) area: WindowHitArea,
}

pub(crate) struct WindowManager<const WINDOW_COUNT: usize> {
    order: [WindowId; WINDOW_COUNT],
    focused: Option<WindowId>,
    captured: Option<WindowId>,
    minimized: [bool; WINDOW_COUNT],
}

impl<const WINDOW_COUNT: usize> WindowManager<WINDOW_COUNT> {
    pub(crate) const fn new(order: [WindowId; WINDOW_COUNT], focused: Option<WindowId>) -> Self {
        Self {
            order,
            focused,
            captured: None,
            minimized: [false; WINDOW_COUNT],
        }
    }

    pub(crate) fn ordered_windows(&self) -> &[WindowId; WINDOW_COUNT] {
        &self.order
    }

    pub(crate) fn focused_window(&self) -> Option<WindowId> {
        self.focused
    }

    pub(crate) fn is_minimized(&self, window_id: WindowId) -> bool {
        self.minimized.get(window_id).copied().unwrap_or(false)
    }

    pub(crate) fn is_visible(&self, window_id: WindowId) -> bool {
        !self.is_minimized(window_id)
    }

    pub(crate) fn focus(&mut self, window_id: WindowId) -> bool {
        if self.is_minimized(window_id) {
            return false;
        }
        if self.focused == Some(window_id) {
            return false;
        }

        self.focused = Some(window_id);
        true
    }

    pub(crate) fn capture(&mut self, window_id: WindowId) {
        self.captured = Some(window_id);
    }

    pub(crate) fn release_capture(&mut self) -> Option<WindowId> {
        self.captured.take()
    }

    pub(crate) fn captured_window(&self) -> Option<WindowId> {
        self.captured
    }

    pub(crate) fn bring_to_front(&mut self, window_id: WindowId) -> bool {
        let front_index = self.order.len().saturating_sub(1);
        if self.order[front_index] == window_id {
            return false;
        }

        let mut found_index = None;
        for order_index in 0..self.order.len() {
            if self.order[order_index] == window_id {
                found_index = Some(order_index);
                break;
            }
        }

        let Some(found_index) = found_index else {
            return false;
        };

        for order_index in found_index..front_index {
            self.order[order_index] = self.order[order_index + 1];
        }
        self.order[front_index] = window_id;
        true
    }

    pub(crate) fn activate(&mut self, window_id: WindowId) -> bool {
        let mut changed = false;
        changed |= self.restore(window_id);
        changed |= self.bring_to_front(window_id);
        changed |= self.focus(window_id);
        changed
    }

    pub(crate) fn minimize(&mut self, window_id: WindowId) -> bool {
        let Some(minimized) = self.minimized.get_mut(window_id) else {
            return false;
        };
        if *minimized {
            return false;
        }

        *minimized = true;
        if self.focused == Some(window_id) {
            self.focused = self.frontmost_visible_window();
        }
        if self.captured == Some(window_id) {
            self.captured = None;
        }
        true
    }

    pub(crate) fn restore(&mut self, window_id: WindowId) -> bool {
        let Some(minimized) = self.minimized.get_mut(window_id) else {
            return false;
        };
        if !*minimized {
            return false;
        }

        *minimized = false;
        true
    }

    pub(crate) fn handle_taskbar_click(&mut self, window_id: WindowId) -> bool {
        if self.is_minimized(window_id) {
            return self.activate(window_id);
        }
        if self.focused == Some(window_id) {
            return self.minimize(window_id);
        }
        self.activate(window_id)
    }

    pub(crate) fn hit_test(
        &self,
        x: usize,
        y: usize,
        mut hit_test_window: impl FnMut(WindowId, usize, usize) -> Option<WindowHitArea>,
    ) -> Option<WindowHit> {
        for order_index in (0..self.order.len()).rev() {
            let window_id = self.order[order_index];
            if self.is_minimized(window_id) {
                continue;
            }
            let Some(area) = hit_test_window(window_id, x, y) else {
                continue;
            };
            return Some(WindowHit { window_id, area });
        }

        None
    }

    fn frontmost_visible_window(&self) -> Option<WindowId> {
        for order_index in (0..self.order.len()).rev() {
            let window_id = self.order[order_index];
            if !self.is_minimized(window_id) {
                return Some(window_id);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::WindowManager;

    #[test]
    fn minimizing_focused_window_moves_focus_to_frontmost_visible_window() {
        let mut manager = WindowManager::<3>::new([0, 1, 2], Some(2));

        assert!(manager.minimize(2));
        assert_eq!(manager.focused_window(), Some(1));
        assert!(manager.is_minimized(2));
    }

    #[test]
    fn activating_minimized_window_restores_focus_and_front_order() {
        let mut manager = WindowManager::<3>::new([0, 1, 2], Some(1));

        assert!(manager.minimize(2));
        assert!(manager.activate(2));
        assert_eq!(manager.focused_window(), Some(2));
        assert_eq!(manager.ordered_windows()[2], 2);
        assert!(!manager.is_minimized(2));
    }
}
