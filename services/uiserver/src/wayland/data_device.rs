//! wl_data_device_manager: clipboard selection and drag-and-drop.
//!
//! Ownership: `uiserver` owns the selection policy. The compositor never reads
//! clipboard bytes; `wl_data_offer.receive` hands the receiving client's pipe
//! straight to the source client through `wl_data_source.send`, so no payload
//! crosses the compositor.
//!
//! Lifecycle: the current selection is one `wl_data_source`. It is offered to
//! the keyboard-focused client when it gains focus or binds a data device, and
//! it is dropped when its source is destroyed, replaced, or cleared.
//!
//! Drag-and-drop follows the pointer grab and negotiates offers and actions.
//! Every recipient data device gets its own offer, sharing terminal arbitration.

use super::*;
use std::os::fd::AsFd;
use std::sync::Weak;
use wayland_server::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
};

pub(super) const MAX_WAYLAND_DATA_SOURCES_PER_CLIENT: usize = 16;
pub(super) const MAX_WAYLAND_DATA_DEVICES_PER_CLIENT: usize = 4;
pub(super) const MAX_WAYLAND_DATA_OFFERS_PER_CLIENT: usize = 64;
pub(super) const MAX_WAYLAND_DATA_MIME_TYPES: usize = 64;
pub(super) const MAX_WAYLAND_DATA_MIME_BYTES: usize = 256;

pub(super) struct DataSourceData {
    mimes: Mutex<Vec<String>>,
    usage: Mutex<SourceUsage>,
    drag_terminal: Mutex<Option<Weak<Mutex<DragTerminal>>>>,
    _ticket: LiveObjectTicket,
}

#[derive(Default)]
struct SourceUsage {
    used: bool,
    actions: Option<u32>,
}

pub(super) struct DataOfferData {
    source: wl_data_source::WlDataSource,
    epoch: u64,
    client_id: ClientId,
    _ticket: LiveObjectTicket,
}

pub(super) struct DragOfferData {
    source: wl_data_source::WlDataSource,
    state: Arc<Mutex<Option<Arc<Mutex<DragOffer>>>>>,
    _ticket: LiveObjectTicket,
}

pub(super) struct DataDeviceData {
    offers: Mutex<Vec<Weak<Mutex<DragOffer>>>>,
    _ticket: LiveObjectTicket,
}

#[derive(Clone)]
pub(super) struct DataDeviceResource {
    resource: wl_data_device::WlDataDevice,
    client_id: ClientId,
}

#[derive(Clone)]
pub(super) struct Selection {
    source: wl_data_source::WlDataSource,
}

pub(super) struct DragState {
    origin_client: ClientId,
    origin: wl_surface::WlSurface,
    source: Option<wl_data_source::WlDataSource>,
    icon: Option<wl_surface::WlSurface>,
    focus: Option<DragFocus>,
    terminal: Arc<Mutex<DragTerminal>>,
    feedback_action: u32,
    feedback_mime: Option<String>,
}

struct DragFocus {
    surface: wl_surface::WlSurface,
    devices: Vec<DragDevice>,
}

struct DragDevice {
    device: wl_data_device::WlDataDevice,
    offer: Option<Arc<Mutex<DragOffer>>>,
}

struct DragTerminal {
    closed: bool,
    source: Option<wl_data_source::WlDataSource>,
}

struct DragOffer {
    terminal: Arc<Mutex<DragTerminal>>,
    serial: u32,
    valid: bool,
    dropped: bool,
    dropped_ask: bool,
    finished: bool,
    accepted_mime: Option<String>,
    source_actions: u32,
    action: u32,
    action_sent: bool,
    version: u32,
}

// Only the selected offer is dropped. Legacy offers have no finish request:
// destruction or disconnect ends their access and completes the transfer.
fn retire_drag_offer(offer: &mut DragOffer, legacy_destroyed: bool) {
    if !offer.valid {
        return;
    }
    offer.valid = false;
    if offer.dropped {
        if let Ok(mut terminal) = offer.terminal.lock() {
            if !terminal.closed {
                terminal.closed = true;
                if let Some(source) = &terminal.source {
                    if source.is_alive() && source.version() >= 3 {
                        if offer.finished || (legacy_destroyed && offer.version < 3) {
                            source.dnd_finished();
                        } else {
                            source.cancelled();
                        }
                    }
                }
            }
        }
    }
}

// Stable device creation order breaks ties; modern eligible offers outrank legacy.
fn drag_winner(drag: &DragState) -> Option<usize> {
    let focus = drag.focus.as_ref()?;
    if drag.source.is_none() {
        return focus
            .devices
            .iter()
            .position(|entry| entry.device.is_alive());
    }
    for modern in [true, false] {
        for (index, entry) in focus.devices.iter().enumerate() {
            if !entry.device.is_alive() {
                continue;
            }
            let eligible = entry
                .offer
                .as_ref()
                .and_then(|offer| offer.lock().ok())
                .is_some_and(|offer| {
                    offer.valid
                        && offer.action != 0
                        && (offer.version >= 3) == modern
                        && (!modern || offer.accepted_mime.is_some())
                });
            if eligible {
                return Some(index);
            }
        }
    }
    None
}

fn refresh_drag_feedback(drag: &mut DragState) {
    let selected = drag_winner(drag).and_then(|index| {
        drag.focus.as_ref()?.devices[index]
            .offer
            .as_ref()?
            .lock()
            .ok()
            .map(|offer| (offer.action, offer.accepted_mime.clone()))
    });
    let (action, mime) = selected.unwrap_or((0, None));
    if let Some(source) = &drag.source {
        if source.is_alive() {
            if drag.feedback_mime != mime {
                source.target(mime.clone());
            }
            if drag.feedback_action != action && source.version() >= 3 {
                source.action(dnd_action(action));
            }
        }
    }
    drag.feedback_action = action;
    drag.feedback_mime = mime;
}

fn leave_drag_focus(focus: DragFocus) {
    for device in focus.devices {
        if device.device.is_alive() {
            device.device.leave();
        }
        if let Some(offer) = device.offer {
            if let Ok(mut offer) = offer.lock() {
                retire_drag_offer(&mut offer, false);
            }
        }
    }
}

fn clear_drag_target(drag: &mut DragState) {
    if let Some(source) = &drag.source {
        if source.is_alive() {
            if drag.feedback_mime.is_some() {
                source.target(None);
            }
            if drag.feedback_action != 0 && source.version() >= 3 {
                source.action(dnd_action(0));
            }
        }
    }
    drag.feedback_mime = None;
    drag.feedback_action = 0;
}

fn dnd_action(bits: u32) -> wl_data_device_manager::DndAction {
    wl_data_device_manager::DndAction::from_bits_truncate(bits)
}

/// Admit one more advertised MIME type, or refuse it. Pure so the limits are
/// testable without a display.
pub(super) fn admit_mime_type(existing: &[String], mime: &str) -> bool {
    !mime.is_empty()
        && mime.len() <= MAX_WAYLAND_DATA_MIME_BYTES
        && !mime.contains('\0')
        && existing.len() < MAX_WAYLAND_DATA_MIME_TYPES
}

impl WaylandState {
    pub(super) fn record_input_serial(&mut self, serial: u32, client: ClientId) {
        if self.input_serials.len() == 64 {
            self.input_serials.remove(0);
        }
        self.input_serials.push((serial, client));
    }

    pub(super) fn drag_motion(&mut self) {
        let hit = self.hit_test_drag(self.pointer_x, self.pointer_y);
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        if let Some(icon) = &drag.icon {
            if let Some(data) = icon.data::<SurfaceData>() {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.frame.x = i64::from(self.pointer_x)
                        .saturating_add(surface.drag_icon_offset_x)
                        .max(0) as usize;
                    surface.frame.y = i64::from(self.pointer_y)
                        .saturating_add(surface.drag_icon_offset_y)
                        .max(0) as usize;
                }
            }
            self.mark_dirty();
        }
        let hit = hit.filter(|hit| drag.source.is_some() || hit.client_id == drag.origin_client);
        let same = match (&drag.focus, &hit) {
            (Some(focus), Some(hit)) => focus.surface.id() == hit.surface.id(),
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some(old) = drag.focus.take() {
                leave_drag_focus(old);
                clear_drag_target(&mut drag);
            }
        }
        if let Some(hit) = &hit {
            if drag.focus.is_none() {
                drag.focus = Some(DragFocus {
                    surface: hit.surface.clone(),
                    devices: Vec::new(),
                });
            }
            let focus = drag.focus.as_mut().unwrap();
            focus.devices.retain(|entry| entry.device.is_alive());
            let devices: Vec<_> = self
                .data_devices
                .iter()
                .filter(|d| {
                    d.client_id == hit.client_id
                        && d.resource.is_alive()
                        && !focus
                            .devices
                            .iter()
                            .any(|entry| entry.device.id() == d.resource.id())
                })
                .map(|d| d.resource.clone())
                .collect();
            for device in devices {
                let serial = self.next_configure_serial();
                let mut offer_resource = None;
                let mut offer_state = None;
                if let Some(source) = &drag.source {
                    let Some(handle) = &self.display_handle else {
                        continue;
                    };
                    let Some(client) = device.client() else {
                        continue;
                    };
                    let Some(ticket) = client.get_data::<WaylandClientState>().and_then(|counts| {
                        LiveObjectTicket::admit(
                            &counts.data_offers,
                            MAX_WAYLAND_DATA_OFFERS_PER_CLIENT,
                            None,
                        )
                    }) else {
                        continue;
                    };
                    let source_actions = source
                        .data::<DataSourceData>()
                        .and_then(|d| d.usage.lock().ok().and_then(|u| u.actions))
                        .unwrap_or(1);
                    let slot = Arc::new(Mutex::new(None));
                    let Ok(offer) = client
                        .create_resource::<wl_data_offer::WlDataOffer, DragOfferData, WaylandState>(
                            handle,
                            device.version(),
                            DragOfferData {
                                source: source.clone(),
                                state: slot.clone(),
                                _ticket: ticket,
                            },
                        )
                    else {
                        continue;
                    };
                    let state = Arc::new(Mutex::new(DragOffer {
                        terminal: drag.terminal.clone(),
                        serial,
                        valid: true,
                        dropped: false,
                        dropped_ask: false,
                        finished: false,
                        accepted_mime: None,
                        source_actions,
                        action: if offer.version() < 3 {
                            source_actions & 1
                        } else {
                            0
                        },
                        action_sent: false,
                        version: offer.version(),
                    }));
                    if let Ok(mut lock) = slot.lock() {
                        *lock = Some(state.clone());
                    }
                    if let Some(data) = device.data::<DataDeviceData>() {
                        if let Ok(mut offers) = data.offers.lock() {
                            offers.retain(|offer| offer.strong_count() != 0);
                            offers.push(Arc::downgrade(&state));
                        }
                    }
                    device.data_offer(&offer);
                    if let Some(data) = source.data::<DataSourceData>() {
                        if let Ok(mimes) = data.mimes.lock() {
                            for mime in mimes.iter() {
                                offer.offer(mime.clone());
                            }
                        }
                    }
                    if offer.version() >= 3 {
                        offer.source_actions(dnd_action(source_actions));
                    }
                    offer_resource = Some(offer);
                    offer_state = Some(state);
                }
                device.enter(
                    serial,
                    &hit.surface,
                    hit.surface_x,
                    hit.surface_y,
                    offer_resource.as_ref(),
                );
                focus.devices.push(DragDevice {
                    device,
                    offer: offer_state,
                });
            }
            if same {
                for entry in &focus.devices {
                    entry
                        .device
                        .motion(self.event_time_ms(), hit.surface_x, hit.surface_y);
                }
            }
        }
        refresh_drag_feedback(&mut drag);
        self.drag = Some(drag);
    }

    pub(super) fn cancel_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if let Ok(mut terminal) = drag.terminal.lock() {
            terminal.closed = true;
        }
        if let Some(focus) = drag.focus {
            leave_drag_focus(focus);
        }
        if let Some(source) = drag.source {
            if source.is_alive() && source.version() >= 3 {
                source.cancelled();
            }
        }
        self.deactivate_drag_icon(drag.icon);
    }

    fn deactivate_drag_icon(&mut self, icon: Option<wl_surface::WlSurface>) {
        if let Some(icon) = icon {
            if let Some(data) = icon.data::<SurfaceData>() {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.drag_icon_active = false;
                }
            }
            self.mark_dirty();
        }
    }

    pub(super) fn drag_surface_destroyed(&mut self, id: &ObjectId) {
        if self
            .drag
            .as_ref()
            .is_some_and(|drag| drag.origin.id() == *id)
        {
            self.cancel_drag();
            return;
        }
        if let Some(drag) = self.drag.as_mut() {
            if drag.icon.as_ref().is_some_and(|icon| icon.id() == *id) {
                drag.icon = None;
            }
            if drag
                .focus
                .as_ref()
                .is_some_and(|focus| focus.surface.id() == *id)
            {
                if let Some(focus) = drag.focus.take() {
                    leave_drag_focus(focus);
                }
                clear_drag_target(drag);
            }
        }
    }

    pub(super) fn drop_drag(&mut self) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };
        refresh_drag_feedback(&mut drag);
        let winner = drag_winner(&drag);
        if let Some(focus) = drag.focus {
            for (index, entry) in focus.devices.into_iter().enumerate() {
                let selected = Some(index) == winner;
                if let Some(offer) = entry.offer {
                    if let Ok(mut offer) = offer.lock() {
                        if selected {
                            offer.dropped = true;
                            offer.dropped_ask = offer.action == 4;
                        } else {
                            retire_drag_offer(&mut offer, false);
                        }
                    }
                }
                if entry.device.is_alive() {
                    if selected {
                        entry.device.drop();
                    }
                    entry.device.leave();
                }
            }
        }
        if winner.is_none() {
            if let Ok(mut terminal) = drag.terminal.lock() {
                terminal.closed = true;
            }
        }
        if let Some(source) = drag.source {
            if source.is_alive() && source.version() >= 3 {
                if winner.is_some() {
                    source.dnd_drop_performed();
                } else {
                    source.cancelled();
                }
            }
        }
        self.deactivate_drag_icon(drag.icon);
    }

    fn keyboard_focus_client(&self) -> Option<ClientId> {
        self.keyboard_focus
            .as_ref()
            .map(|focus| focus.client_id.clone())
    }

    /// Replace the selection and notify the focused client.
    fn replace_selection(&mut self, next: Option<wl_data_source::WlDataSource>) {
        self.invalidate_selection_offers();
        let previous = self.selection.take();
        if let Some(previous) = previous {
            let same = next
                .as_ref()
                .is_some_and(|next| next.id() == previous.source.id());
            if !same && previous.source.is_alive() {
                previous.source.cancelled();
            }
        }
        self.selection = next.map(|source| Selection { source });
        if let Some(client_id) = self.keyboard_focus_client() {
            self.send_selection_to_client(&client_id);
        }
    }

    pub(super) fn invalidate_selection_offers(&mut self) {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
    }

    /// Offer the current selection (or its absence) on every data device the
    /// client owns. Called on keyboard enter and when the selection changes.
    pub(super) fn send_selection_to_client(&mut self, client_id: &ClientId) {
        self.data_devices
            .retain(|device| device.resource.is_alive());
        let Some(handle) = self.display_handle.clone() else {
            return;
        };
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.source.is_alive())
        {
            self.selection = None;
        }
        let devices: Vec<_> = self
            .data_devices
            .iter()
            .filter(|device| &device.client_id == client_id)
            .cloned()
            .collect();
        for device in devices {
            Self::offer_selection(
                &handle,
                &device.resource,
                self.selection.as_ref(),
                self.selection_epoch,
                client_id,
            );
        }
    }

    fn offer_selection(
        handle: &DisplayHandle,
        device: &wl_data_device::WlDataDevice,
        selection: Option<&Selection>,
        epoch: u64,
        client_id: &ClientId,
    ) {
        let Some(selection) = selection else {
            device.selection(None);
            return;
        };
        let offer = device.client().and_then(|client| {
            let ticket = client.get_data::<WaylandClientState>().and_then(|counts| {
                LiveObjectTicket::admit(
                    &counts.data_offers,
                    MAX_WAYLAND_DATA_OFFERS_PER_CLIENT,
                    None,
                )
            })?;
            client
                .create_resource::<wl_data_offer::WlDataOffer, DataOfferData, WaylandState>(
                    handle,
                    device.version(),
                    DataOfferData {
                        source: selection.source.clone(),
                        epoch,
                        client_id: client_id.clone(),
                        _ticket: ticket,
                    },
                )
                .ok()
        });
        let Some(offer) = offer else {
            // Offer budget exhausted: present an empty clipboard rather than
            // a stale one.
            device.selection(None);
            return;
        };
        device.data_offer(&offer);
        if let Some(data) = selection.source.data::<DataSourceData>() {
            if let Ok(mimes) = data.mimes.lock() {
                for mime in mimes.iter() {
                    offer.offer(mime.clone());
                }
            }
        }
        device.selection(Some(&offer));
    }
}

impl GlobalDispatch<wl_data_device_manager::WlDataDeviceManager, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_data_device_manager::WlDataDeviceManager>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &wl_data_device_manager::WlDataDeviceManager,
        request: wl_data_device_manager::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                let Some(ticket) = client.get_data::<WaylandClientState>().and_then(|counts| {
                    LiveObjectTicket::admit(
                        &counts.data_sources,
                        MAX_WAYLAND_DATA_SOURCES_PER_CLIENT,
                        None,
                    )
                }) else {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_data_device_manager.create_data_source: limit \
                             {MAX_WAYLAND_DATA_SOURCES_PER_CLIENT} reached"
                        ),
                    );
                    return;
                };
                data_init.init(
                    id,
                    DataSourceData {
                        mimes: Mutex::new(Vec::new()),
                        usage: Mutex::new(SourceUsage::default()),
                        drag_terminal: Mutex::new(None),
                        _ticket: ticket,
                    },
                );
            }
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                let Some(ticket) = client.get_data::<WaylandClientState>().and_then(|counts| {
                    LiveObjectTicket::admit(
                        &counts.data_devices,
                        MAX_WAYLAND_DATA_DEVICES_PER_CLIENT,
                        None,
                    )
                }) else {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_data_device_manager.get_data_device: limit \
                             {MAX_WAYLAND_DATA_DEVICES_PER_CLIENT} reached"
                        ),
                    );
                    return;
                };
                let device = data_init.init(
                    id,
                    DataDeviceData {
                        offers: Mutex::new(Vec::new()),
                        _ticket: ticket,
                    },
                );
                let client_id = client.id();
                state.data_devices.push(DataDeviceResource {
                    resource: device.clone(),
                    client_id: client_id.clone(),
                });
                if state.keyboard_focus_client().as_ref() == Some(&client_id) {
                    if let Some(handle) = state.display_handle.as_ref() {
                        Self::offer_selection(
                            handle,
                            &device,
                            state.selection.as_ref(),
                            state.selection_epoch,
                            &client_id,
                        );
                    }
                }
                if state.drag.is_some() {
                    state.drag_motion();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, DataSourceData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wl_data_source::WlDataSource,
        request: wl_data_source::Request,
        data: &DataSourceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_source::Request::Offer { mime_type } => {
                let Ok(mut mimes) = data.mimes.lock() else {
                    return;
                };
                if !admit_mime_type(&mimes, &mime_type) {
                    drop(mimes);
                    post_protocol_error(
                        resource,
                        "wl_data_source.offer: MIME type rejected (empty, oversize or too many)"
                            .into(),
                    );
                    return;
                }
                if !mimes.contains(&mime_type) {
                    mimes.push(mime_type);
                }
            }
            wl_data_source::Request::SetActions { dnd_actions } => {
                let bits = match dnd_actions {
                    WEnum::Value(actions) => actions.bits(),
                    WEnum::Unknown(bits) => bits,
                };
                if bits & !7 != 0 {
                    resource.post_error(
                        wl_data_source::Error::InvalidActionMask,
                        "invalid drag action mask".to_string(),
                    );
                    return;
                }
                if let Ok(mut usage) = data.usage.lock() {
                    if usage.used || usage.actions.is_some() {
                        resource.post_error(
                            wl_data_source::Error::InvalidSource,
                            "source actions already set or source used".to_string(),
                        );
                    } else {
                        usage.actions = Some(bits);
                    }
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &wl_data_source::WlDataSource,
        data: &DataSourceData,
    ) {
        if let Some(terminal) = data
            .drag_terminal
            .lock()
            .ok()
            .and_then(|t| t.as_ref().and_then(Weak::upgrade))
        {
            if let Ok(mut terminal) = terminal.lock() {
                terminal.closed = true;
            }
        }
        if state.drag.as_ref().is_some_and(|drag| {
            drag.source
                .as_ref()
                .is_some_and(|source| source.id() == resource.id())
        }) {
            state.cancel_drag();
        }
        let owns = state
            .selection
            .as_ref()
            .is_some_and(|selection| selection.source.id() == resource.id());
        if owns {
            // The source is already dead; do not `cancelled` it again.
            state.selection = None;
            state.invalidate_selection_offers();
            if let Some(client_id) = state.keyboard_focus_client() {
                state.send_selection_to_client(&client_id);
            }
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, DataDeviceData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &wl_data_device::WlDataDevice,
        request: wl_data_device::Request,
        _data: &DataDeviceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device::Request::SetSelection { source, serial } => {
                if state.keyboard_focus_client().as_ref() != Some(&client.id())
                    || !state
                        .input_serials
                        .iter()
                        .any(|(sent, owner)| *sent == serial && *owner == client.id())
                    || state
                        .selection_serial
                        .is_some_and(|previous| (serial.wrapping_sub(previous) as i32) < 0)
                {
                    return;
                }
                if let Some(source) = source.as_ref() {
                    let Some(data) = source.data::<DataSourceData>() else {
                        return;
                    };
                    let Ok(mut usage) = data.usage.lock() else {
                        return;
                    };
                    if usage.used {
                        resource.post_error(
                            wl_data_device::Error::UsedSource,
                            "source has already been used".to_string(),
                        );
                        return;
                    }
                    if usage.actions.is_some() {
                        source.post_error(
                            wl_data_source::Error::InvalidSource,
                            "drag source cannot be used as a selection".to_string(),
                        );
                        return;
                    }
                    usage.used = true;
                }
                state.selection_serial = Some(serial);
                state.replace_selection(source);
            }
            wl_data_device::Request::StartDrag {
                source,
                origin,
                icon,
                serial,
            } => {
                let valid_grab = state.pointer_button_down
                    && state.pointer_grab_serial == Some(serial)
                    && state.pointer_focus.as_ref().is_some_and(|focus| {
                        focus.client_id == client.id() && focus.surface.id() == origin.id()
                    });
                if !valid_grab || state.drag.is_some() {
                    return;
                }
                if let Some(source) = &source {
                    let Some(data) = source.data::<DataSourceData>() else {
                        return;
                    };
                    let Ok(mut usage) = data.usage.lock() else {
                        return;
                    };
                    if usage.used {
                        resource.post_error(
                            wl_data_device::Error::UsedSource,
                            "source has already been used".to_string(),
                        );
                        return;
                    }
                    usage.used = true;
                }
                if let Some(icon) = &icon {
                    let Some(data) = icon.data::<SurfaceData>() else {
                        return;
                    };
                    let Ok(mut surface) = data.shared.lock() else {
                        return;
                    };
                    if surface.role_assigned && !surface.drag_icon_role {
                        resource.post_error(
                            wl_data_device::Error::Role,
                            "drag icon already has a role".to_string(),
                        );
                        return;
                    }
                    surface.role_assigned = true;
                    surface.drag_icon_role = true;
                    surface.drag_icon_active = true;
                    surface.drag_icon_offset_x = 0;
                    surface.drag_icon_offset_y = 0;
                    surface.decoration_server = false;
                    surface.decoration_ack_server = false;
                    surface.decoration_requested_server = false;
                    surface.frame.x = state.pointer_x as usize;
                    surface.frame.y = state.pointer_y as usize;
                }
                let terminal = Arc::new(Mutex::new(DragTerminal {
                    closed: false,
                    source: source.clone(),
                }));
                if let Some(data) = source
                    .as_ref()
                    .and_then(|source| source.data::<DataSourceData>())
                {
                    if let Ok(mut slot) = data.drag_terminal.lock() {
                        *slot = Some(Arc::downgrade(&terminal));
                    }
                }
                state.update_pointer_focus(None);
                state.drag = Some(DragState {
                    origin_client: client.id(),
                    origin,
                    source,
                    icon,
                    focus: None,
                    terminal,
                    feedback_action: 0,
                    feedback_mime: None,
                });
                state.drag_motion();
            }
            wl_data_device::Request::Release => {
                state
                    .data_devices
                    .retain(|device| device.resource.is_alive());
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &wl_data_device::WlDataDevice,
        data: &DataDeviceData,
    ) {
        state
            .data_devices
            .retain(|device| device.resource.id() != resource.id());
        if let Ok(mut offers) = data.offers.lock() {
            for offer in offers.drain(..).filter_map(|offer| offer.upgrade()) {
                if let Ok(mut offer) = offer.lock() {
                    if !offer.dropped {
                        retire_drag_offer(&mut offer, false);
                    }
                }
            }
        }
        if let Some(drag) = state.drag.as_mut() {
            if let Some(focus) = drag.focus.as_mut() {
                focus
                    .devices
                    .retain(|entry| entry.device.id() != resource.id());
                refresh_drag_feedback(drag);
            }
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, DataOfferData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_data_offer::WlDataOffer,
        request: wl_data_offer::Request,
        data: &DataOfferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_offer::Request::Finish => resource.post_error(
                wl_data_offer::Error::InvalidFinish,
                "finish is invalid for a selection offer".to_string(),
            ),
            wl_data_offer::Request::SetActions { .. } => resource.post_error(
                wl_data_offer::Error::InvalidOffer,
                "set_actions is invalid for a selection offer".to_string(),
            ),
            wl_data_offer::Request::Receive { mime_type, fd } => {
                if !data.source.is_alive()
                    || data.epoch != state.selection_epoch
                    || state.keyboard_focus_client().as_ref() != Some(&data.client_id)
                    || !state
                        .selection
                        .as_ref()
                        .is_some_and(|selection| selection.source.id() == data.source.id())
                {
                    // The owner is gone; closing the pipe tells the receiver EOF.
                    return;
                }
                let advertised = data
                    .source
                    .data::<DataSourceData>()
                    .and_then(|source| source.mimes.lock().ok().map(|m| m.contains(&mime_type)))
                    .unwrap_or(false);
                if advertised {
                    data.source.send(mime_type, fd.as_fd());
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, DragOfferData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_data_offer::WlDataOffer,
        request: wl_data_offer::Request,
        data: &DragOfferData,
        _handle: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        let Some(offer) = data.state.lock().ok().and_then(|slot| slot.clone()) else {
            return;
        };
        let Ok(mut offer) = offer.lock() else {
            return;
        };
        let closed = offer
            .terminal
            .lock()
            .map_or(true, |terminal| terminal.closed);
        if closed || !data.source.is_alive() {
            if matches!(request, wl_data_offer::Request::Finish) {
                resource.post_error(
                    wl_data_offer::Error::InvalidFinish,
                    "drag is no longer active".to_string(),
                );
            }
            return;
        }
        if offer.finished {
            if !matches!(request, wl_data_offer::Request::Destroy) {
                resource.post_error(
                    wl_data_offer::Error::InvalidFinish,
                    "offer already finished".to_string(),
                );
            }
            return;
        }
        match request {
            wl_data_offer::Request::Accept { serial, mime_type } => {
                if !offer.valid || serial != offer.serial {
                    return;
                }
                let accepted = mime_type.as_ref().is_some_and(|mime| {
                    data.source
                        .data::<DataSourceData>()
                        .and_then(|source| source.mimes.lock().ok().map(|m| m.contains(mime)))
                        .unwrap_or(false)
                });
                offer.accepted_mime = if accepted { mime_type } else { None };
                let dropped = offer.dropped;
                let mime = offer.accepted_mime.clone();
                drop(offer);
                if dropped {
                    if data.source.is_alive() {
                        data.source.target(mime);
                    }
                } else if let Some(drag) = state.drag.as_mut() {
                    refresh_drag_feedback(drag);
                }
            }
            wl_data_offer::Request::Receive { mime_type, fd } => {
                if offer.valid
                    && data.source.is_alive()
                    && data
                        .source
                        .data::<DataSourceData>()
                        .and_then(|source| source.mimes.lock().ok().map(|m| m.contains(&mime_type)))
                        .unwrap_or(false)
                {
                    data.source.send(mime_type, fd.as_fd());
                }
            }
            wl_data_offer::Request::SetActions {
                dnd_actions,
                preferred_action,
            } => {
                let actions = match dnd_actions {
                    WEnum::Value(v) => v.bits(),
                    WEnum::Unknown(v) => v,
                };
                let preferred = match preferred_action {
                    WEnum::Value(v) => v.bits(),
                    WEnum::Unknown(v) => v,
                };
                if actions & !7 != 0 {
                    resource.post_error(
                        wl_data_offer::Error::InvalidActionMask,
                        "invalid drag action mask".to_string(),
                    );
                    return;
                }
                if preferred & !7 != 0
                    || preferred.count_ones() > 1
                    || preferred & !actions != 0
                    || (offer.dropped
                        && offer.action == 4
                        && preferred != 0
                        && preferred & offer.source_actions == 0)
                {
                    resource.post_error(
                        wl_data_offer::Error::InvalidAction,
                        "invalid preferred drag action".to_string(),
                    );
                    return;
                }
                if !offer.valid || (offer.dropped && offer.action != 4) {
                    return;
                }
                let intersection = actions & offer.source_actions;
                let action = if preferred & intersection != 0 {
                    preferred
                } else {
                    intersection & intersection.wrapping_neg()
                };
                let offer_changed = !offer.action_sent || offer.action != action;
                let dropped = offer.dropped;
                offer.action = action;
                offer.action_sent = true;
                if offer_changed && !dropped {
                    resource.action(dnd_action(action));
                }
                drop(offer);
                if !dropped {
                    if let Some(drag) = state.drag.as_mut() {
                        refresh_drag_feedback(drag);
                    }
                }
            }
            wl_data_offer::Request::Finish => {
                if !offer.valid
                    || !offer.dropped
                    || offer.accepted_mime.is_none()
                    || offer.action == 0
                    || offer.action == 4
                {
                    resource.post_error(
                        wl_data_offer::Error::InvalidFinish,
                        "drag cannot finish in current state".to_string(),
                    );
                    return;
                }
                if offer.dropped_ask && data.source.is_alive() && data.source.version() >= 3 {
                    // The final ask action must immediately precede dnd_finished.
                    data.source.action(dnd_action(offer.action));
                }
                offer.finished = true;
                retire_drag_offer(&mut offer, false);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        _resource: &wl_data_offer::WlDataOffer,
        data: &DragOfferData,
    ) {
        if let Some(offer) = data.state.lock().ok().and_then(|slot| slot.clone()) {
            if let Ok(mut offer) = offer.lock() {
                retire_drag_offer(&mut offer, true);
            }
        }
        if let Some(drag) = state.drag.as_mut() {
            refresh_drag_feedback(drag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_admission_bounds_count_length_and_content() {
        assert!(admit_mime_type(&[], "text/plain;charset=utf-8"));
        assert!(!admit_mime_type(&[], ""));
        assert!(!admit_mime_type(
            &[],
            &"x".repeat(MAX_WAYLAND_DATA_MIME_BYTES + 1)
        ));
        assert!(!admit_mime_type(&[], "text/pl\0ain"));
        let full: Vec<String> = (0..MAX_WAYLAND_DATA_MIME_TYPES)
            .map(|index| format!("application/x-{index}"))
            .collect();
        assert!(!admit_mime_type(&full, "text/plain"));
        assert!(admit_mime_type(&full[1..], "text/plain"));
    }
}
