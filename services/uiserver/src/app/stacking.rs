//! One back-to-front identity order shared by rendering and input policy.

use super::AppState;
use crate::sys::ConsoleSessionHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowId {
    Console(ConsoleSessionHandle),
    Wayland(u32),
}

#[derive(Default)]
pub(crate) struct WindowStack(Vec<WindowId>);

impl WindowStack {
    #[cfg(test)]
    fn reconcile(&mut self, live: &[WindowId]) {
        self.0.retain(|id| live.contains(id));
        for id in live {
            if !self.0.contains(id) {
                self.0.push(*id);
            }
        }
    }

    fn raise(&mut self, id: WindowId) -> bool {
        if self.0.last() == Some(&id) {
            return false;
        }
        self.0.retain(|entry| *entry != id);
        self.0.push(id);
        true
    }

    fn visit_draw_order(
        &self,
        consoles: &[super::ConsoleWindow],
        wayland: &[crate::wayland::WaylandWindowSnapshot],
        focused_console: ConsoleSessionHandle,
        focused_wayland: Option<u32>,
        mut visit: impl FnMut(WindowId),
    ) {
        let focused = if let Some(id) = focused_wayland {
            wayland
                .iter()
                .any(|w| w.surface_id == id && !w.minimized)
                .then_some(WindowId::Wayland(id))
        } else if consoles
            .iter()
            .any(|w| w.session_handle == focused_console && !w.minimized)
        {
            Some(WindowId::Console(focused_console))
        } else {
            None
        };
        let is_overlay = |id| match id {
            WindowId::Console(_) => false,
            WindowId::Wayland(id) => wayland
                .iter()
                .any(|window| window.surface_id == id && (window.popup || window.drag_icon)),
        };
        let is_live = |id| match id {
            WindowId::Console(id) => consoles.iter().any(|w| w.session_handle == id),
            WindowId::Wayland(id) => wayland.iter().any(|w| w.surface_id == id),
        };
        for &id in &self.0 {
            if is_live(id) && Some(id) != focused && !is_overlay(id) {
                visit(id);
            }
        }
        // Newly announced identities follow the same console-then-Wayland
        // append order as WindowStack::reconcile before any raising occurs.
        for id in consoles
            .iter()
            .map(|window| WindowId::Console(window.session_handle))
            .chain(
                wayland
                    .iter()
                    .map(|window| WindowId::Wayland(window.surface_id)),
            )
        {
            if !self.0.contains(&id) && Some(id) != focused && !is_overlay(id) {
                visit(id);
            }
        }
        if let Some(id) = focused.filter(|id| !is_overlay(*id)) {
            visit(id);
        }
        for popup in wayland
            .iter()
            .filter(|window| window.popup && !window.drag_icon)
        {
            visit(WindowId::Wayland(popup.surface_id));
        }
        for icon in wayland
            .iter()
            .filter(|window| window.drag_icon)
        {
            visit(WindowId::Wayland(icon.surface_id));
        }
    }
}

/// Resolve only the top painted frame: chrome and consoles occlude lower
/// Wayland client areas just as opaque frame pixels do.
pub(crate) fn window_hit_test(
    order: &[WindowId],
    consoles: &[super::ConsoleWindow],
    wayland: &[crate::wayland::WaylandWindowSnapshot],
    x: u32,
    y: u32,
) -> Option<(WindowId, crate::layout::WindowChromeHit)> {
    order.iter().rev().find_map(|id| {
        if let WindowId::Wayland(id) = *id {
            if wayland.iter().any(|w| w.surface_id == id && w.drag_icon) {
                return None;
            }
        }
        let outer = match *id {
            WindowId::Console(id) => {
                consoles
                    .iter()
                    .find(|w| w.session_handle == id && !w.minimized)?
                    .frame
            }
            WindowId::Wayland(id) => crate::render::wayland_window_outer_rect(
                wayland
                    .iter()
                    .find(|w| w.surface_id == id && !w.minimized)?,
            ),
        };
        outer.contains(x, y).then(|| {
            let client_only = match *id {
                WindowId::Wayland(id) => wayland
                    .iter()
                    .find(|w| w.surface_id == id)
                    .is_some_and(|w| !w.server_decoration),
                _ => false,
            };
            (
                *id,
                if client_only {
                    crate::layout::WindowChromeHit::Client
                } else {
                    crate::layout::window_chrome_hit(outer, x, y)
                },
            )
        })
    })
}

/// Avoid identical titlebar origins, including when protocol owners announce
/// windows in the reverse order. Try the established cascade, then distinct
/// legal origins; if the output admits only occupied origins, keep a legal one.
pub(crate) fn place_window(
    width: u32,
    height: u32,
    frame: crate::canvas::Rect,
    wayland: bool,
    occupied: &[crate::canvas::Rect],
) -> crate::canvas::Rect {
    use crate::layout;
    let bounds = layout::desktop_bounds(width, height);
    let clamp = |rect| {
        if wayland {
            layout::clamp_wayland_frame(rect, width, height)
        } else {
            layout::clamp_window_rect(width, height, rect)
        }
    };
    let frame = clamp(frame);
    let free = |rect: crate::canvas::Rect| {
        !occupied
            .iter()
            .any(|other| (other.x, other.y) == (rect.x, rect.y))
    };
    if free(frame) {
        return frame;
    }
    for index in 0..=occupied.len() {
        let (x, y) = layout::cascade_origin(bounds, index);
        let candidate = clamp(crate::canvas::Rect { x, y, ..frame });
        if free(candidate) {
            return candidate;
        }
    }
    let min = clamp(crate::canvas::Rect {
        x: bounds.x,
        y: bounds.y,
        ..frame
    });
    let max = clamp(crate::canvas::Rect {
        x: bounds.x + bounds.width,
        y: bounds.y + bounds.height,
        ..frame
    });
    let columns = max.x.saturating_sub(min.x) + 1;
    let rows = max.y.saturating_sub(min.y) + 1;
    // At most N+1 distinct candidates can be occupied by N windows.
    for index in 0..columns.saturating_mul(rows).min(occupied.len() + 1) {
        let candidate = clamp(crate::canvas::Rect {
            x: min.x + index % columns,
            y: min.y + index / columns,
            ..frame
        });
        if free(candidate) {
            return candidate;
        }
    }
    frame
}

impl AppState {
    pub(super) fn occupied_window_frames(&self) -> Vec<crate::canvas::Rect> {
        self.console_windows
            .iter()
            .map(|w| w.frame)
            .chain(self.wayland_placement_frames())
            .collect()
    }

    pub(super) fn wayland_placement_frames(&self) -> Vec<crate::canvas::Rect> {
        self.wayland_windows
            .iter()
            .filter(|w| !w.drag_icon && !w.popup)
            .map(|w| w.frame)
            .chain(
                self.pending_wayland_placements
                    .iter()
                    .filter_map(|(id, x, y)| {
                        self.wayland_windows
                            .iter()
                            .find(|w| w.surface_id == *id)
                            .map(|w| crate::canvas::Rect {
                                x: *x,
                                y: *y,
                                ..w.frame
                            })
                    }),
            )
            .collect()
    }

    pub(super) fn cancel_wayland_placement(&mut self, surface_id: u32) {
        self.pending_wayland_placements
            .retain(|(id, _, _)| *id != surface_id);
    }

    pub(super) fn plan_new_wayland_placements(
        &mut self,
        windows: &[crate::wayland::WaylandWindowSnapshot],
    ) {
        self.pending_wayland_placements.retain(|(id, x, y)| {
            windows
                .iter()
                .any(|w| w.surface_id == *id && (w.frame.x, w.frame.y) != (*x, *y))
        });
        let mut occupied = self.occupied_window_frames();
        for window in windows {
            if window.drag_icon || window.popup {
                continue;
            }
            if self
                .wayland_windows
                .iter()
                .any(|old| old.surface_id == window.surface_id)
            {
                continue;
            }
            let frame = place_window(
                self.display.width,
                self.display.height,
                window.frame,
                true,
                &occupied,
            );
            if frame != window.frame {
                self.pending_wayland_placements
                    .push((window.surface_id, frame.x, frame.y));
            }
            occupied.push(frame);
        }
    }

    /// Requests are retired only by a matching geometry snapshot or removal,
    /// not by enqueue success. The IO owner applies them to exact surface IDs.
    pub(crate) fn pending_wayland_placements(&self) -> &[(u32, usize, usize)] {
        &self.pending_wayland_placements
    }

    pub(super) fn reconcile_window_stack(&mut self) {
        self.window_stack = WindowStack(self.window_draw_order());
    }

    pub(super) fn raise_window(&mut self, id: WindowId) -> bool {
        let was_top = self.window_stack.0.last() == Some(&id);
        self.reconcile_window_stack();
        self.window_stack.raise(id);
        !was_top
    }

    pub(super) fn focus_wayland_window(&mut self, surface_id: u32) {
        self.raise_window(WindowId::Wayland(surface_id));
        self.focused_wayland_surface_id = Some(surface_id);
        self.focused_session_handle = 0;
        // A queued console acknowledgement must not steal explicit UI focus.
        self.pending_console_focus = None;
    }

    /// Total back-to-front order. Minimized windows retain their place, but
    /// are omitted by consumers. Reconcile also tolerates a fresh snapshot
    /// before its lifecycle reconciliation turn.
    pub(crate) fn window_draw_order(&self) -> Vec<WindowId> {
        let mut order = Vec::with_capacity(self.console_windows.len() + self.wayland_windows.len());
        self.visit_window_draw_order(|id| order.push(id));
        order
    }

    /// Visit the same reconciled order without allocating a temporary stack
    /// or live-identity list. Focus is raised before popups, then drag icons;
    /// identities in those groups are omitted from the ordinary stack pass.
    pub(crate) fn visit_window_draw_order(&self, visit: impl FnMut(WindowId)) {
        self.window_stack.visit_draw_order(
            &self.console_windows,
            &self.wayland_windows,
            self.focused_session_handle,
            self.focused_wayland_surface_id,
            visit,
        );
    }

    pub(super) fn top_visible_window(&self) -> Option<WindowId> {
        self.window_draw_order()
            .into_iter()
            .rev()
            .find(|id| match id {
                WindowId::Console(id) => self
                    .console_windows
                    .iter()
                    .any(|w| w.session_handle == *id && !w.minimized),
                WindowId::Wayland(id) => self
                    .wayland_windows
                    .iter()
                    .any(|w| w.surface_id == *id && !w.minimized && !w.drag_icon && !w.popup),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        WindowId,
        WindowId::{Console, Wayland},
        WindowStack,
    };
    use crate::app::ConsoleWindow;
    use crate::canvas::Rect;
    use crate::sys::ConsoleSessionHandle;
    use crate::wayland::WaylandWindowSnapshot;
    use std::sync::Arc;

    // Keep the original clone/reconcile/raise algorithm as an independent
    // ordering oracle for the allocation-free visitor.
    fn reference_draw_order(
        stack: &WindowStack,
        consoles: &[ConsoleWindow],
        wayland: &[WaylandWindowSnapshot],
        focused_console: ConsoleSessionHandle,
        focused_wayland: Option<u32>,
    ) -> Vec<WindowId> {
        let live: Vec<_> = consoles
            .iter()
            .map(|window| Console(window.session_handle))
            .chain(wayland.iter().map(|window| Wayland(window.surface_id)))
            .collect();
        let mut order = WindowStack(stack.0.clone());
        order.reconcile(&live);
        if let Some(id) = focused_wayland {
            if wayland
                .iter()
                .any(|window| window.surface_id == id && !window.minimized)
            {
                order.raise(Wayland(id));
            }
        } else if consoles
            .iter()
            .any(|window| window.session_handle == focused_console && !window.minimized)
        {
            order.raise(Console(focused_console));
        }
        for window in wayland.iter().filter(|window| window.popup) {
            order.raise(Wayland(window.surface_id));
        }
        for window in wayland.iter().filter(|window| window.drag_icon) {
            order.raise(Wayland(window.surface_id));
        }
        order.0
    }

    fn ordering_window(id: u32, popup: bool, drag_icon: bool) -> WaylandWindowSnapshot {
        WaylandWindowSnapshot {
            surface_id: id,
            title: format!("window-{id}"),
            frame: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            width: 1,
            height: 1,
            stride_pixels: 1,
            pixels: Arc::new(vec![0xff12_3456]),
            content_version: 1,
            damage: Rect::empty(),
            minimized: false,
            server_decoration: true,
            drag_icon,
            popup,
        }
    }

    #[test]
    fn draw_order_visitor_matches_reconciliation_focus_and_overlay_reference() {
        let mut consoles: Vec<_> = (1..=3)
            .map(|id| ConsoleWindow::new(id, format!("console-{id}"), Rect::empty(), 1))
            .collect();
        let mut wayland = vec![
            ordering_window(1, false, false),
            ordering_window(2, true, false),
            ordering_window(3, false, true),
            ordering_window(4, true, true),
            ordering_window(5, false, false),
        ];
        let stacks = [
            vec![],
            vec![Console(99), Wayland(99)],
            vec![
                Wayland(4),
                Console(2),
                Wayland(1),
                Console(1),
                Wayland(3),
                Wayland(5),
            ],
            vec![
                Console(3),
                Wayland(2),
                Console(2),
                Wayland(5),
                Wayland(1),
                Console(1),
                Wayland(3),
                Wayland(4),
            ],
            vec![
                Wayland(5),
                Wayland(4),
                Wayland(3),
                Wayland(2),
                Wayland(1),
                Console(3),
                Console(2),
                Console(1),
            ],
        ];
        for rotation in 0..wayland.len() {
            for minimized in [0_u32, 0b10101, 0b11111] {
                for window in &mut wayland {
                    window.minimized = minimized & (1 << (window.surface_id - 1)) != 0;
                }
                for window in &mut consoles {
                    window.minimized = minimized & (1 << (window.session_handle - 1)) != 0;
                }
                for entries in &stacks {
                    let stack = WindowStack(entries.clone());
                    for focused_console in [0, 1, 2, 3, 99] {
                        for focused_wayland in
                            [None, Some(1), Some(2), Some(3), Some(4), Some(5), Some(99)]
                        {
                            let expected = reference_draw_order(
                                &stack,
                                &consoles,
                                &wayland,
                                focused_console,
                                focused_wayland,
                            );
                            let mut actual = Vec::new();
                            stack.visit_draw_order(
                                &consoles,
                                &wayland,
                                focused_console,
                                focused_wayland,
                                |id| actual.push(id),
                            );
                            assert_eq!(
                                actual, expected,
                                "rotation={rotation} minimized={minimized:b} console={focused_console} wayland={focused_wayland:?} stack={entries:?}",
                            );
                            assert_eq!(&stack.0, entries, "visiting must not change raise history");
                        }
                    }
                }
            }
            wayland.rotate_left(1);
        }
    }

    #[test]
    fn mixed_stack_preserves_raise_history_across_focus_changes() {
        let mut stack = WindowStack::default();
        stack.reconcile(&[Console(1), Console(2), Wayland(1), Wayland(2)]);
        stack.raise(Console(1));
        stack.raise(Wayland(1));
        assert_eq!(stack.0, [Console(2), Wayland(2), Console(1), Wayland(1)]);
        stack.raise(Console(2));
        assert_eq!(stack.0, [Wayland(2), Console(1), Wayland(1), Console(2)]);
    }

    #[test]
    fn mixed_stack_prunes_destroyed_ids_and_appends_recreated_windows_once() {
        let mut stack = WindowStack::default();
        stack.reconcile(&[Console(1), Wayland(1)]);
        stack.raise(Console(1));
        stack.reconcile(&[Wayland(1)]);
        stack.reconcile(&[Console(1), Wayland(1)]);
        stack.reconcile(&[Console(1), Wayland(1)]);
        assert_eq!(stack.0, [Wayland(1), Console(1)]);
    }
}

#[cfg(test)]
mod placement_tests {
    use super::place_window;
    use crate::{canvas::Rect, layout};

    fn wayland_frame() -> Rect {
        let bounds = layout::desktop_bounds(2560, 1440);
        Rect {
            x: bounds.x,
            y: bounds.y,
            width: 800,
            height: 520,
        }
    }

    #[test]
    fn console_then_wayland_and_wayland_then_console_have_distinct_titlebars() {
        let console = layout::default_console_window_rect(2560, 1440, 0);
        let wayland = wayland_frame();
        assert_eq!(
            (console.x, console.y),
            (wayland.x, wayland.y),
            "both owners initially choose origin zero"
        );
        let after_console = place_window(2560, 1440, wayland, true, &[console]);
        assert_ne!((after_console.x, after_console.y), (console.x, console.y));
        let after_wayland = place_window(2560, 1440, console, false, &[wayland]);
        assert_ne!((after_wayland.x, after_wayland.y), (wayland.x, wayland.y));
    }

    #[test]
    fn mapping_after_an_unmapped_wayland_allocation_rechecks_shared_placement() {
        let allocated = wayland_frame(); // server allocated before any consoles existed
        let console = layout::default_console_window_rect(2560, 1440, 0);
        let mapped = place_window(2560, 1440, allocated, true, &[console]);
        let second_mapping = place_window(2560, 1440, allocated, true, &[console, mapped]);
        assert_ne!((mapped.x, mapped.y), (console.x, console.y));
        assert_ne!((second_mapping.x, second_mapping.y), (mapped.x, mapped.y));
        assert_eq!(
            place_window(2560, 1440, mapped, true, &[console]),
            mapped,
            "acknowledged placement is stable"
        );
    }

    #[test]
    fn shared_placement_does_not_repeat_when_the_old_cascade_wraps() {
        let mut occupied = Vec::new();
        for _ in 0..64 {
            let frame = place_window(2560, 1440, wayland_frame(), true, &occupied);
            assert!(
                occupied
                    .iter()
                    .all(|other: &Rect| (other.x, other.y) != (frame.x, frame.y))
            );
            occupied.push(frame);
        }
    }
}
