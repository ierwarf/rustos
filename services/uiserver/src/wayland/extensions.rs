//! Chrome/modern-client Wayland protocol extensions:
//! - `wp_content_type_manager_v1` / `wp_content_type_v1`
//! - `wp_fractional_scale_manager_v1` / `wp_fractional_scale_v1`
//! - `wp_cursor_shape_manager_v1` / `wp_cursor_shape_device_v1`
//! - `wp_single_pixel_buffer_manager_v1`
//! - `xdg_activation_v1` / `xdg_activation_token_v1`
//! - `zwp_idle_inhibit_manager_v1` / `zwp_idle_inhibitor_v1`
//! - `ext_idle_notifier_v1` / `ext_idle_notification_v1`

use super::*;
use wayland_protocols::ext::idle_notify::v1::server::{
    ext_idle_notification_v1, ext_idle_notifier_v1,
};
use wayland_protocols::wp::content_type::v1::server::{
    wp_content_type_manager_v1, wp_content_type_v1,
};
use wayland_protocols::wp::cursor_shape::v1::server::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::fractional_scale::v1::server::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::idle_inhibit::zv1::server::{
    zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1,
};
use wayland_protocols::wp::single_pixel_buffer::v1::server::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::xdg::activation::v1::server::{
    xdg_activation_token_v1, xdg_activation_v1,
};

const MAX_WAYLAND_ACTIVATION_TOKENS: usize = 64;
const MAX_WAYLAND_IDLE_NOTIFICATIONS: usize = 64;
const MAX_WAYLAND_IDLE_INHIBITORS: usize = 64;

pub(super) struct ContentTypeData {
    surface: SurfaceData,
}

pub(super) struct FractionalScaleData {
    surface: SurfaceData,
}

pub(super) struct CursorShapeDeviceData;

pub(super) struct IdleInhibitorData;

pub(super) struct IdleNotificationData;

pub(super) struct IdleNotificationEntry {
    pub(super) resource: ext_idle_notification_v1::ExtIdleNotificationV1,
    pub(super) timeout_ms: u32,
    pub(super) input_only: bool,
    pub(super) idled: bool,
}

pub(super) struct IdleInhibitorEntry {
    pub(super) resource: zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
    pub(super) surface: Arc<Mutex<WaylandSurfaceState>>,
}

pub(super) struct ActivationTokenState {
    used: bool,
    serial: Option<u32>,
    app_id: Option<String>,
    surface_id: Option<u32>,
}

pub(super) struct ActivationTokenData {
    state: Mutex<ActivationTokenState>,
}

#[derive(Clone, Debug)]
pub(super) struct StoredActivationToken {
    pub(super) token: String,
    _serial: Option<u32>,
    _app_id: Option<String>,
    _source_surface_id: Option<u32>,
}

/// Convert 32-bit UNORM premultiplied RGBA components (the wire format of
/// `wp_single_pixel_buffer_manager_v1`) to compositor BGRA8888 (`0xAA_RR_GG_BB`).
pub(super) fn single_pixel_rgba_to_bgra8(r: u32, g: u32, b: u32, a: u32) -> u32 {
    let a8 = a >> 24;
    let r8 = (r >> 24).min(a8);
    let g8 = (g >> 24).min(a8);
    let b8 = (b >> 24).min(a8);
    (a8 << 24) | (r8 << 16) | (g8 << 8) | b8
}

// ---- wp_content_type_manager_v1 -------------------------------------------

impl GlobalDispatch<wp_content_type_manager_v1::WpContentTypeManagerV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wp_content_type_manager_v1::WpContentTypeManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wp_content_type_manager_v1::WpContentTypeManagerV1, ()> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wp_content_type_manager_v1::WpContentTypeManagerV1,
        request: wp_content_type_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_content_type_manager_v1::Request::GetSurfaceContentType { id, surface } =
            request
        {
            let Some(surface_data) = surface.data::<SurfaceData>().cloned() else {
                post_protocol_error(
                    resource,
                    "wp_content_type_manager_v1: unknown wl_surface".into(),
                );
                return;
            };
            let already = match surface_data.shared.lock() {
                Ok(mut s) => {
                    if s.content_type_constructed {
                        true
                    } else {
                        s.content_type_constructed = true;
                        false
                    }
                }
                Err(_) => true,
            };
            if already {
                resource.post_error(
                    wp_content_type_manager_v1::Error::AlreadyConstructed,
                    "surface already has a wp_content_type_v1 object".to_string(),
                );
                return;
            }
            data_init.init(id, ContentTypeData { surface: surface_data });
        }
    }
}

impl Dispatch<wp_content_type_v1::WpContentTypeV1, ContentTypeData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_content_type_v1::WpContentTypeV1,
        request: wp_content_type_v1::Request,
        data: &ContentTypeData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_content_type_v1::Request::SetContentType { content_type } => {
                if let WEnum::Value(value) = content_type {
                    if let Ok(mut s) = data.surface.shared.lock() {
                        s.content_type = value as u32;
                    }
                }
            }
            wp_content_type_v1::Request::Destroy => {
                if let Ok(mut s) = data.surface.shared.lock() {
                    s.content_type_constructed = false;
                    s.content_type = 0;
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: wayland_server::backend::ClientId,
        _resource: &wp_content_type_v1::WpContentTypeV1,
        data: &ContentTypeData,
    ) {
        if let Ok(mut s) = data.surface.shared.lock() {
            s.content_type_constructed = false;
        }
    }
}

// ---- wp_fractional_scale_manager_v1 ---------------------------------------

impl GlobalDispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()>
    for WaylandState
{
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        request: wp_fractional_scale_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_fractional_scale_manager_v1::Request::GetFractionalScale { id, surface } =
            request
        {
            let Some(surface_data) = surface.data::<SurfaceData>().cloned() else {
                post_protocol_error(
                    resource,
                    "wp_fractional_scale_manager_v1: unknown wl_surface".into(),
                );
                return;
            };
            let exists = match surface_data.shared.lock() {
                Ok(mut s) => {
                    if s.fractional_scale_constructed {
                        true
                    } else {
                        s.fractional_scale_constructed = true;
                        false
                    }
                }
                Err(_) => true,
            };
            if exists {
                resource.post_error(
                    wp_fractional_scale_manager_v1::Error::FractionalScaleExists,
                    "surface already has a wp_fractional_scale_v1 object".to_string(),
                );
                return;
            }
            let scale_120 = (state.output_scale.max(1) as u32).saturating_mul(120);
            let obj = data_init.init(id, FractionalScaleData { surface: surface_data });
            obj.preferred_scale(scale_120);
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, FractionalScaleData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_fractional_scale_v1::WpFractionalScaleV1,
        request: wp_fractional_scale_v1::Request,
        data: &FractionalScaleData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, wp_fractional_scale_v1::Request::Destroy) {
            if let Ok(mut s) = data.surface.shared.lock() {
                s.fractional_scale_constructed = false;
            }
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: wayland_server::backend::ClientId,
        _resource: &wp_fractional_scale_v1::WpFractionalScaleV1,
        data: &FractionalScaleData,
    ) {
        if let Ok(mut s) = data.surface.shared.lock() {
            s.fractional_scale_constructed = false;
        }
    }
}

// ---- wp_cursor_shape_manager_v1 -------------------------------------------

impl GlobalDispatch<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, ()> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
        request: wp_cursor_shape_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_cursor_shape_manager_v1::Request::GetPointer {
                cursor_shape_device,
                pointer: _,
            } => {
                data_init.init(cursor_shape_device, CursorShapeDeviceData);
            }
            wp_cursor_shape_manager_v1::Request::GetTabletToolV2 {
                cursor_shape_device,
                tablet_tool: _,
            } => {
                data_init.init(cursor_shape_device, CursorShapeDeviceData);
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1, CursorShapeDeviceData>
    for WaylandState
{
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
        request: wp_cursor_shape_device_v1::Request,
        _data: &CursorShapeDeviceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_cursor_shape_device_v1::Request::SetShape { serial: _, shape } = request {
            match shape {
                WEnum::Value(shape) => {
                    if resource.version() < 2
                        && matches!(
                            shape,
                            wp_cursor_shape_device_v1::Shape::DndAsk
                                | wp_cursor_shape_device_v1::Shape::AllResize
                        )
                    {
                        resource.post_error(
                            wp_cursor_shape_device_v1::Error::InvalidShape,
                            "cursor shape requires wp_cursor_shape_manager_v1 v2".to_string(),
                        );
                        return;
                    }
                    state.cursor_shape = shape as u32;
                }
                WEnum::Unknown(_) => {
                    resource.post_error(
                        wp_cursor_shape_device_v1::Error::InvalidShape,
                        "unknown wp_cursor_shape_device_v1 shape".to_string(),
                    );
                }
            }
        }
    }
}

// ---- wp_single_pixel_buffer_manager_v1 ------------------------------------

impl GlobalDispatch<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1, ()>
    for WaylandState
{
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1, ()>
    for WaylandState
{
    fn request(
        _state: &mut Self,
        client: &Client,
        resource: &wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
        request: wp_single_pixel_buffer_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_single_pixel_buffer_manager_v1::Request::CreateU32RgbaBuffer {
            id,
            r,
            g,
            b,
            a,
        } = request
        {
            let Some(ticket) = client
                .get_data::<WaylandClientState>()
                .and_then(|counts| {
                    LiveObjectTicket::admit(&counts.buffers, MAX_WAYLAND_BUFFERS_PER_CLIENT, None)
                })
            else {
                post_protocol_error(
                    resource,
                    format!(
                        "wp_single_pixel_buffer_manager_v1: buffer limit {MAX_WAYLAND_BUFFERS_PER_CLIENT} reached"
                    ),
                );
                return;
            };
            let pixel = single_pixel_rgba_to_bgra8(r, g, b, a);
            data_init.init(
                id,
                BufferData {
                    shared: Arc::new(WaylandBufferState {
                        pool: None,
                        solid_pixel: Some(pixel),
                        offset: 0,
                        width: 1,
                        height: 1,
                        stride: 4,
                        has_alpha: (pixel >> 24) != 0xff,
                        app_dmabuf: false,
                        _ticket: ticket,
                    }),
                },
            );
        }
    }
}

// ---- xdg_activation_v1 ----------------------------------------------------

impl GlobalDispatch<xdg_activation_v1::XdgActivationV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<xdg_activation_v1::XdgActivationV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_activation_v1::XdgActivationV1,
        request: xdg_activation_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_activation_v1::Request::GetActivationToken { id } => {
                data_init.init(
                    id,
                    ActivationTokenData {
                        state: Mutex::new(ActivationTokenState {
                            used: false,
                            serial: None,
                            app_id: None,
                            surface_id: None,
                        }),
                    },
                );
            }
            xdg_activation_v1::Request::Activate { token, surface } => {
                if let Some(pos) = state
                    .activation_tokens
                    .iter()
                    .position(|entry| entry.token == token)
                {
                    let _ = state.activation_tokens.remove(pos);
                    if let Some(surface_data) = surface.data::<SurfaceData>() {
                        if let Ok(target) = surface_data.shared.lock() {
                            let target_id = target.surface_id;
                            let is_mapped = target.alive
                                && target.toplevel.is_some()
                                && !target.needs_initial_configure
                                && !target.pixels.is_empty();
                            drop(target);
                            if is_mapped {
                                let _ = state.focus_surface(target_id);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ActivationTokenData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &xdg_activation_token_v1::XdgActivationTokenV1,
        request: xdg_activation_token_v1::Request,
        data: &ActivationTokenData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Ok(mut token_state) = data.state.lock() else {
            return;
        };
        if token_state.used
            && !matches!(request, xdg_activation_token_v1::Request::Destroy)
        {
            resource.post_error(
                xdg_activation_token_v1::Error::AlreadyUsed,
                "xdg_activation_token_v1 already committed".to_string(),
            );
            return;
        }
        match request {
            xdg_activation_token_v1::Request::SetSerial { serial, seat: _ } => {
                token_state.serial = Some(serial);
            }
            xdg_activation_token_v1::Request::SetAppId { app_id } => {
                if app_id.len() <= 256 {
                    token_state.app_id = Some(app_id);
                }
            }
            xdg_activation_token_v1::Request::SetSurface { surface } => {
                token_state.surface_id = surface
                    .data::<SurfaceData>()
                    .and_then(|s| s.shared.lock().ok().map(|st| st.surface_id));
            }
            xdg_activation_token_v1::Request::Commit => {
                token_state.used = true;
                let seq = state.next_activation_token_seq.max(1);
                state.next_activation_token_seq = seq.wrapping_add(1).max(1);
                let token = format!("rustos-act-{seq:016x}");
                if state.activation_tokens.len() >= MAX_WAYLAND_ACTIVATION_TOKENS {
                    state.activation_tokens.pop_front();
                }
                state.activation_tokens.push_back(StoredActivationToken {
                    token: token.clone(),
                    _serial: token_state.serial,
                    _app_id: token_state.app_id.clone(),
                    _source_surface_id: token_state.surface_id,
                });
                drop(token_state);
                resource.done(token);
            }
            _ => {}
        }
    }
}

// ---- zwp_idle_inhibit_manager_v1 & ext_idle_notifier_v1 -------------------

impl GlobalDispatch<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
        request: zwp_idle_inhibit_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_idle_inhibit_manager_v1::Request::CreateInhibitor { id, surface } = request {
            let Some(surface_data) = surface.data::<SurfaceData>().cloned() else {
                post_protocol_error(
                    resource,
                    "zwp_idle_inhibit_manager_v1: unknown wl_surface".into(),
                );
                return;
            };
            state
                .idle_inhibitors
                .retain(|entry| entry.resource.is_alive());
            if state.idle_inhibitors.len() >= MAX_WAYLAND_IDLE_INHIBITORS {
                post_protocol_error(
                    resource,
                    "zwp_idle_inhibit_manager_v1: inhibitor limit reached".into(),
                );
                return;
            }
            let obj = data_init.init(id, IdleInhibitorData);
            state.idle_inhibitors.push(IdleInhibitorEntry {
                resource: obj,
                surface: surface_data.shared,
            });
            state.note_input_activity();
        }
    }
}

impl Dispatch<zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1, IdleInhibitorData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
        request: zwp_idle_inhibitor_v1::Request,
        _data: &IdleInhibitorData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, zwp_idle_inhibitor_v1::Request::Destroy) {
            state
                .idle_inhibitors
                .retain(|entry| entry.resource.id() != resource.id());
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
        _data: &IdleInhibitorData,
    ) {
        state
            .idle_inhibitors
            .retain(|entry| entry.resource.id() != resource.id());
    }
}

impl GlobalDispatch<ext_idle_notifier_v1::ExtIdleNotifierV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ext_idle_notifier_v1::ExtIdleNotifierV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ext_idle_notifier_v1::ExtIdleNotifierV1, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ext_idle_notifier_v1::ExtIdleNotifierV1,
        request: ext_idle_notifier_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let (id, timeout, input_only) = match request {
            ext_idle_notifier_v1::Request::GetIdleNotification {
                id,
                timeout,
                seat: _,
            } => (id, timeout, false),
            ext_idle_notifier_v1::Request::GetInputIdleNotification {
                id,
                timeout,
                seat: _,
            } => (id, timeout, true),
            _ => return,
        };
        state
            .idle_notifications
            .retain(|entry| entry.resource.is_alive());
        if state.idle_notifications.len() >= MAX_WAYLAND_IDLE_NOTIFICATIONS {
            post_protocol_error(
                resource,
                "ext_idle_notifier_v1: notification limit reached".into(),
            );
            return;
        }
        let obj = data_init.init(id, IdleNotificationData);
        state.idle_notifications.push(IdleNotificationEntry {
            resource: obj,
            timeout_ms: timeout,
            input_only,
            idled: false,
        });
        state.update_idle_notifications(Instant::now());
    }
}

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, IdleNotificationData>
    for WaylandState
{
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ext_idle_notification_v1::ExtIdleNotificationV1,
        request: ext_idle_notification_v1::Request,
        _data: &IdleNotificationData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, ext_idle_notification_v1::Request::Destroy) {
            state
                .idle_notifications
                .retain(|entry| entry.resource.id() != resource.id());
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &ext_idle_notification_v1::ExtIdleNotificationV1,
        _data: &IdleNotificationData,
    ) {
        state
            .idle_notifications
            .retain(|entry| entry.resource.id() != resource.id());
    }
}

impl WaylandState {
    pub(super) fn note_input_activity(&mut self) {
        let now = Instant::now();
        self.last_input_activity = now;
        for entry in &mut self.idle_notifications {
            if entry.resource.is_alive() && entry.idled {
                entry.idled = false;
                entry.resource.resumed();
            }
        }
    }

    pub(super) fn has_active_idle_inhibitor(&mut self) -> bool {
        self.idle_inhibitors.retain(|entry| {
            entry.resource.is_alive()
                && entry
                    .surface
                    .lock()
                    .is_ok_and(|s| s.alive && !s.pixels.is_empty())
        });
        !self.idle_inhibitors.is_empty()
    }

    pub(super) fn update_idle_notifications(&mut self, now: Instant) {
        let inhibited = self.has_active_idle_inhibitor();
        let elapsed_ms = now
            .saturating_duration_since(self.last_input_activity)
            .as_millis() as u64;
        self.idle_notifications
            .retain(|entry| entry.resource.is_alive());
        for entry in &mut self.idle_notifications {
            let blocked = !entry.input_only && inhibited;
            let should_idle = !blocked && elapsed_ms >= u64::from(entry.timeout_ms);
            if should_idle && !entry.idled {
                entry.idled = true;
                entry.resource.idled();
            } else if !should_idle && entry.idled {
                entry.idled = false;
                entry.resource.resumed();
            }
        }
    }
}
