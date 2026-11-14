//! wl_surface.commit as a non-blocking transaction.
//!
//! A commit that needs client pixels is *planned* on the UI thread, read and
//! converted on a snapshot worker (`snapshot.rs`), and *applied* on the UI
//! thread when the worker reports. While one is in flight, later commits on the
//! same surface queue and coalesce (newest attachment wins, damage unions), so
//! per-surface order is preserved and a slow client cannot grow an unbounded
//! backlog. This is the same shape wlroots calls a locked pending state and
//! Mutter a transaction: a commit becomes visible only once its content
//! dependency is satisfied. Nothing in this file waits on a client fd.

use super::snapshot::{blit_damage, Completion, JobId, Refused};
use super::viewport::{map_buffer, ViewportError};
use super::*;

/// The state a `wl_surface.commit` carries, captured at request time.
pub(super) struct PendingCommit {
    /// `Some(None)` is a null attach (unmap); `None` is "no attach request".
    pub(super) attach: Option<Option<BufferAttachment>>,
    pub(super) damage: Rect,
    pub(super) viewport: ViewportState,
    pub(super) mapping: BufferMapping,
    pub(super) decoration_server: bool,
    pub(super) offset: (i64, i64),
}

pub(super) struct InFlightCommit {
    job: JobId,
    resource: wl_surface::WlSurface,
    source: BufferData,
    release: Option<wl_buffer::WlBuffer>,
    damage: Rect,
    has_damage: bool,
    started: Instant,
}

#[derive(Default)]
pub(super) struct CommitSlot {
    in_flight: Option<InFlightCommit>,
    queued: Option<PendingCommit>,
}

impl CommitSlot {
    pub(super) fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// Fold a newer commit into a queued older one. Returns the merged commit and
/// the buffer of the superseded attachment, which was never read and can be
/// handed straight back to its client.
pub(super) fn merge_commits(
    old: PendingCommit,
    new: PendingCommit,
) -> (PendingCommit, Option<wl_buffer::WlBuffer>) {
    let damage = old.damage.union(new.damage);
    let offset = (
        old.offset.0.saturating_add(new.offset.0),
        old.offset.1.saturating_add(new.offset.1),
    );
    match new.attach {
        Some(attach) => {
            let released = match old.attach {
                Some(Some(superseded)) => Some(superseded.resource),
                _ => None,
            };
            (
                PendingCommit {
                    attach: Some(attach),
                    damage,
                    viewport: new.viewport,
                    mapping: new.mapping,
                    decoration_server: new.decoration_server,
                    offset,
                },
                released,
            )
        }
        None => (
            PendingCommit {
                attach: old.attach,
                damage,
                viewport: new.viewport,
                mapping: new.mapping,
                decoration_server: new.decoration_server,
                offset,
            },
            None,
        ),
    }
}

/// What a commit turned out to be once it is applied.
#[derive(Default)]
struct Outcome {
    mapped: bool,
    dirty: bool,
    trigger_initial_configure: bool,
    has_damage: bool,
    release: Option<wl_buffer::WlBuffer>,
}

fn initial_configure_due(surface: &WaylandSurfaceState, mapped: bool) -> bool {
    surface.needs_initial_configure && !mapped && surface.toplevel.is_some()
}

impl WaylandState {
    pub(super) fn commit_surface(&mut self, resource: &wl_surface::WlSurface, data: &SurfaceData) {
        let (pending_buffer, damage, viewport, mapping, decoration_server, offset, alive) = {
            let Ok(mut surface) = data.shared.lock() else {
                return;
            };
            let damage = surface.pending_damage;
            surface.pending_damage = Rect::empty();
            let offset = std::mem::take(&mut surface.pending_offset);
            (
                surface.pending_buffer.take(),
                damage,
                surface.pending_viewport,
                surface.pending_buffer_mapping,
                surface.decoration_ack_server,
                (i64::from(offset.0), i64::from(offset.1)),
                surface.alive,
            )
        };
        if !alive {
            // A retired surface (its toplevel was destroyed or it was closed)
            // is never shown again. Retaining a client buffer for it would pin
            // up to 64 MiB until the next prune, so hand the attached buffer
            // straight back.
            if let Some(Some(attachment)) = pending_buffer {
                attachment.resource.release();
            }
            return;
        }
        let commits_buffer = matches!(pending_buffer, Some(Some(_))) || !damage.is_empty();
        if commits_buffer {
            let commit_ready = match data.shared.lock() {
                Ok(surface)
                    if surface.toplevel.is_some()
                        && surface.current_buffer.is_none()
                        && surface.acknowledged_serial == 0 =>
                {
                    Err("wl_surface.commit: xdg surface buffer committed before configure ack")
                }
                Ok(_) => Ok(()),
                Err(_) => Err("wl_surface.commit: surface unavailable"),
            };
            if let Err(message) = commit_ready {
                post_protocol_error(resource, message.into());
                return;
            }
        }
        let commit = PendingCommit {
            attach: pending_buffer,
            damage,
            viewport,
            mapping,
            decoration_server,
            offset,
        };
        // Every commit queues behind an in-flight one, including commits that
        // need no pixels: a null attach processed ahead of an earlier read
        // would be resurrected when that read lands.
        let commit = {
            let Ok(mut surface) = data.shared.lock() else {
                return;
            };
            if surface.commit.is_busy() {
                let merged = match surface.commit.queued.take() {
                    Some(old) => {
                        let (merged, released) = merge_commits(old, commit);
                        if let Some(buffer) = released {
                            surface.pending_buffer_releases.push(buffer);
                        }
                        merged
                    }
                    None => commit,
                };
                surface.commit.queued = Some(merged);
                return;
            }
            commit
        };
        self.run_commit(resource, &data.shared, commit);
    }

    /// Apply a commit now, or plan its read and leave it in flight.
    fn run_commit(
        &mut self,
        resource: &wl_surface::WlSurface,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        commit: PendingCommit,
    ) {
        let PendingCommit {
            attach,
            damage,
            viewport,
            mapping,
            decoration_server,
            offset,
        } = commit;
        let has_damage = !damage.is_empty();
        // Promote the captured mapping. Mapping-only changes are derived from
        // compositor-owned pixels rather than a buffer already released to its client.
        let viewport_changed = shared.lock().is_ok_and(|mut surface| {
            let changed = surface.viewport != viewport || surface.buffer_mapping != mapping;
            surface.viewport = viewport;
            surface.buffer_mapping = mapping;
            changed
        });
        let icon_offset_changed = shared.lock().is_ok_and(|mut surface| {
            if !surface.drag_icon_role || offset == (0, 0) {
                return false;
            }
            surface.drag_icon_offset_x = surface.drag_icon_offset_x.saturating_add(offset.0);
            surface.drag_icon_offset_y = surface.drag_icon_offset_y.saturating_add(offset.1);
            surface.frame.x = (i64::from(self.pointer_x).saturating_add(surface.drag_icon_offset_x))
                .max(0) as usize;
            surface.frame.y = (i64::from(self.pointer_y).saturating_add(surface.drag_icon_offset_y))
                .max(0) as usize;
            surface.drag_icon_active
        });
        let decoration_changed = shared.lock().is_ok_and(|mut surface| {
            let changed = surface.decoration_server != decoration_server;
            surface.decoration_server = decoration_server;
            changed
        });
        let mut outcome = Outcome {
            has_damage,
            dirty: decoration_changed || icon_offset_changed,
            ..Outcome::default()
        };
        let mut copy_source = None::<BufferData>;

        match attach {
            Some(Some(BufferAttachment {
                resource: buffer,
                data,
            })) => {
                copy_source = Some(data);
                outcome.release = Some(buffer);
            }
            Some(None) => {
                if let Ok(mut surface) = shared.lock() {
                    surface.current_buffer = None;
                    surface.pixels = Arc::default();
                    surface.original_pixels = Arc::default();
                    surface.original_width = 0;
                    surface.original_height = 0;
                    surface.width = 0;
                    surface.height = 0;
                    surface.stride_pixels = 0;
                    surface.last_damage = Rect::empty();
                    surface.content_version = next_content_version(surface.content_version);
                    surface.configured_serial = 0;
                    surface.acknowledged_serial = 0;
                    surface.outstanding_configures.clear();
                    surface.decoration_configures.clear();
                    surface.needs_initial_configure = surface.toplevel.is_some();
                    outcome.trigger_initial_configure =
                        surface.needs_initial_configure && surface.toplevel.is_some();
                    outcome.dirty = true;
                }
            }
            None if viewport_changed && !has_damage => {
                let retained = shared.lock().ok().and_then(|surface| {
                    surface.current_buffer.as_ref().map(|_| {
                        (
                            surface.original_pixels.clone(),
                            surface.original_width,
                            surface.original_height,
                        )
                    })
                });
                if let Some((pixels, width, height)) = retained {
                    if !self.present_original(resource, shared, pixels, width, height, &mut outcome)
                    {
                        return;
                    }
                }
            }
            None if has_damage => {
                copy_source = shared
                    .lock()
                    .ok()
                    .and_then(|surface| surface.current_buffer.clone());
            }
            None => {
                if let Ok(surface) = shared.lock() {
                    outcome.mapped =
                        !surface.pixels.is_empty() && surface.width != 0 && surface.height != 0;
                    outcome.trigger_initial_configure = surface.needs_initial_configure
                        && !outcome.mapped
                        && surface.toplevel.is_some();
                }
            }
        }

        if let Some(source) = copy_source {
            // Attaching a same-layout buffer with no declared damage changes
            // buffer ownership, not visible pixels. Preserve the compositor
            // snapshot and skip the read. The first buffer or a layout change
            // still needs a complete copy so no undefined pixels become visible.
            let retained = !has_damage
                && !viewport_changed
                && shared.lock().is_ok_and(|mut surface| {
                    let can_retain = can_retain_undamaged_buffer(
                        !surface.original_pixels.is_empty(),
                        surface.original_width,
                        surface.original_height,
                        surface.original_width,
                        source.shared.width,
                        source.shared.height,
                        source.shared.width,
                    );
                    if can_retain {
                        surface.current_buffer = Some(source.clone());
                        surface.last_damage = Rect::empty();
                        outcome.mapped = true;
                    }
                    can_retain
                });
            if !retained {
                match self.plan_pixels(
                    resource,
                    shared,
                    &source,
                    damage,
                    has_damage && !viewport_changed,
                    &mut outcome,
                ) {
                    Planned::Done => {}
                    Planned::InFlight(spec) => {
                        self.submit_commit(resource, shared, source, spec, damage, outcome);
                        return;
                    }
                    Planned::Abort => return,
                }
            }
        }
        self.finish_commit(shared, outcome);
        self.start_queued_commit(resource, shared);
    }

    /// Decide how a commit's pixels are obtained. Damage that lies outside the
    /// buffer is satisfied without a read.
    fn plan_pixels(
        &mut self,
        resource: &wl_surface::WlSurface,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        source: &BufferData,
        damage: Rect,
        has_damage: bool,
        outcome: &mut Outcome,
    ) -> Planned {
        let viewport_active = shared.lock().is_ok_and(|surface| {
            surface.viewport.is_active() || surface.buffer_mapping != BufferMapping::default()
        });
        // A damage patch is in buffer coordinates; under a viewport the
        // presented pixels are resampled, so only a full read is correct.
        if has_damage && !viewport_active {
            let (width, height, stride, len) = shared.lock().map_or((0, 0, 0, 0), |surface| {
                (
                    surface.width,
                    surface.height,
                    surface.stride_pixels,
                    surface.pixels.len(),
                )
            });
            match source.plan_damage(width, height, stride, len, damage) {
                DamagePlan::Read(spec) => return Planned::InFlight(spec),
                DamagePlan::Empty => {
                    if let Ok(mut surface) = shared.lock() {
                        surface.current_buffer = Some(source.clone());
                        surface.last_damage = damage;
                        surface.content_version = next_content_version(surface.content_version);
                        outcome.mapped = true;
                        outcome.dirty = true;
                        outcome.trigger_initial_configure = initial_configure_due(&surface, true);
                    }
                    self.record_buffer_copy(Duration::ZERO, 0);
                    return Planned::Done;
                }
                DamagePlan::NeedsFull => {}
            }
        }
        let incoming_pixels = source.shared.width.saturating_mul(source.shared.height);
        if !surface_pixel_budget_allows(
            self.retained_surface_pixels_excluding(shared),
            incoming_pixels,
        ) {
            post_protocol_error(
                resource,
                "wl_surface.commit: compositor surface memory budget exceeded".into(),
            );
            return Planned::Abort;
        }
        match source.plan_full() {
            Some(spec) => Planned::InFlight(spec),
            None => {
                resource.post_error(
                    wl_shm::Error::InvalidFd,
                    "wl_surface.commit: unreadable or truncated shm buffer".to_string(),
                );
                Planned::Abort
            }
        }
    }

    fn submit_commit(
        &mut self,
        resource: &wl_surface::WlSurface,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        source: BufferData,
        spec: SnapshotSpec,
        damage: Rect,
        outcome: Outcome,
    ) {
        let Some(client) = shared
            .lock()
            .ok()
            .and_then(|surface| surface.client_id.clone())
        else {
            return;
        };
        match self.snapshots.submit(client, spec) {
            Ok(job) => {
                if let Ok(mut surface) = shared.lock() {
                    surface.commit.in_flight = Some(InFlightCommit {
                        job,
                        resource: resource.clone(),
                        source,
                        release: outcome.release,
                        damage,
                        has_damage: outcome.has_damage,
                        started: Instant::now(),
                    });
                }
            }
            Err(refused) => {
                let reason = match refused {
                    Refused::ClientJobs => "too many snapshots in flight",
                    Refused::ClientBytes => "client snapshot byte budget exceeded",
                    Refused::TotalBytes => "compositor snapshot byte budget exceeded",
                };
                resource.post_error(
                    wl_shm::Error::InvalidFd,
                    format!("wl_surface.commit: {reason}"),
                );
            }
        }
    }

    /// Apply the result of a finished snapshot job.
    fn complete_commit(&mut self, completion: Completion) {
        let Some((shared, flight)) = self.take_in_flight(completion.id) else {
            // Cancelled or retired while the read ran.
            return;
        };
        let mut outcome = Outcome {
            has_damage: flight.has_damage,
            release: flight.release,
            ..Outcome::default()
        };
        let elapsed = flight.started.elapsed();
        match completion.result {
            Ok(SnapshotPixels::Damage { damage, rows }) => {
                let applied = shared.lock().is_ok_and(|mut surface| {
                    let (width, height) = (surface.width, surface.height);
                    if !blit_damage(&mut surface.pixels, width, height, damage, &rows) {
                        return false;
                    }
                    surface.original_pixels = surface.pixels.clone();
                    surface.original_width = width;
                    surface.original_height = height;
                    surface.current_buffer = Some(flight.source.clone());
                    surface.last_damage = flight.damage;
                    surface.content_version = next_content_version(surface.content_version);
                    outcome.mapped = true;
                    outcome.dirty = true;
                    outcome.trigger_initial_configure = initial_configure_due(&surface, true);
                    true
                });
                if !applied {
                    flight.resource.post_error(
                        wl_shm::Error::InvalidFd,
                        "wl_surface.commit: damage no longer fits the committed surface"
                            .to_string(),
                    );
                    return;
                }
                let bytes = flight
                    .damage
                    .width
                    .saturating_mul(flight.damage.height)
                    .saturating_mul(std::mem::size_of::<u32>());
                super::dmabuf::note_snapshot(&flight.source, bytes);
                self.record_buffer_copy(elapsed, bytes);
            }
            Ok(SnapshotPixels::Full {
                pixels,
                width,
                height,
            }) => {
                let bytes = pixels.len().saturating_mul(std::mem::size_of::<u32>());
                if !self.present_original(
                    &flight.resource,
                    &shared,
                    Arc::new(pixels),
                    width,
                    height,
                    &mut outcome,
                ) {
                    return;
                }
                if let Ok(mut surface) = shared.lock() {
                    surface.current_buffer = Some(flight.source.clone());
                }
                super::dmabuf::note_snapshot(&flight.source, bytes);
                self.record_buffer_copy(elapsed, bytes);
            }
            Err(_) => {
                // A bad backing file is a client failure, not a successful
                // commit of the previous pixels.
                flight.resource.post_error(
                    wl_shm::Error::InvalidFd,
                    "wl_surface.commit: unreadable or truncated shm buffer".to_string(),
                );
                return;
            }
        }
        self.finish_commit(&shared, outcome);
        self.start_queued_commit(&flight.resource, &shared);
    }

    fn present_original(
        &mut self,
        resource: &wl_surface::WlSurface,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        original: Arc<Vec<u32>>,
        width: usize,
        height: usize,
        outcome: &mut Outcome,
    ) -> bool {
        let (viewport, mapping, viewport_resource) = match shared.lock() {
            Ok(surface) => (
                surface.viewport,
                surface.buffer_mapping,
                surface.viewport_resource.clone(),
            ),
            Err(_) => return false,
        };
        let (original_width, original_height) = (width, height);
        let (pixels, width, height) = match map_buffer(&original, width, height, mapping, &viewport)
        {
            Ok(None) => (original.clone(), width, height),
            Ok(Some((pixels, width, height))) => (Arc::new(pixels), width, height),
            Err(ViewportError::BufferSize) => {
                resource.post_error(
                    wl_surface::Error::InvalidSize,
                    "buffer dimensions must be a multiple of scale".to_string(),
                );
                return false;
            }
            Err(error) => {
                if let Some(viewport) = viewport_resource {
                    let code = match error {
                        ViewportError::OutOfBuffer => wp_viewport::Error::OutOfBuffer,
                        _ => wp_viewport::Error::BadSize,
                    };
                    viewport.post_error(code, "invalid viewport at commit".to_string());
                } else {
                    post_protocol_error(resource, "invalid viewport at commit".into());
                }
                return false;
            }
        };
        let retained = original
            .len()
            .saturating_add(if Arc::ptr_eq(&original, &pixels) {
                0
            } else {
                pixels.len()
            });
        if !surface_pixel_budget_allows(self.retained_surface_pixels_excluding(shared), retained) {
            post_protocol_error(
                resource,
                "wl_surface.commit: compositor surface memory budget exceeded".into(),
            );
            return false;
        }
        if let Ok(mut surface) = shared.lock() {
            surface.original_pixels = original;
            surface.original_width = original_width;
            surface.original_height = original_height;
            surface.pixels = pixels;
            surface.width = width;
            surface.height = height;
            surface.stride_pixels = width;
            surface.content_version = next_content_version(surface.content_version);
            let (visible_w, visible_h) = wayland_client_size_for_buffer(
                width,
                height,
                self.display_width,
                self.display_height,
            );
            if surface.drag_icon_role {
                surface.frame.width = width;
                surface.frame.height = height;
            } else {
                surface.frame = clamp_wayland_frame(
                    Rect {
                        x: surface.frame.x,
                        y: surface.frame.y,
                        width: visible_w,
                        height: visible_h,
                    },
                    self.display_width,
                    self.display_height,
                );
            }
            surface.last_damage = Rect {
                x: 0,
                y: 0,
                width,
                height,
            };
            outcome.mapped = true;
            outcome.dirty = true;
            outcome.trigger_initial_configure = initial_configure_due(&surface, true);
            true
        } else {
            false
        }
    }

    /// The commit's visible effects, shared by the synchronous and the
    /// completed-snapshot paths.
    fn finish_commit(&mut self, shared: &Arc<Mutex<WaylandSurfaceState>>, mut outcome: Outcome) {
        if let Some(buffer) = outcome.release.take() {
            if let Ok(mut surface) = shared.lock() {
                surface.pending_buffer_releases.push(buffer);
            } else {
                buffer.release();
            }
        }
        if !outcome.mapped {
            if let Ok(surface) = shared.lock() {
                if !surface.pixels.is_empty() && surface.width != 0 && surface.height != 0 {
                    outcome.mapped = true;
                }
            }
        }
        if !outcome.dirty {
            if let Ok(mut surface) = shared.lock() {
                surface.last_damage = Rect::empty();
            }
        }
        if outcome.dirty {
            self.mark_dirty();
        }
        self.sync_surface_output(shared, outcome.mapped);
        if outcome.trigger_initial_configure {
            self.send_toplevel_configure(shared);
        }
    }

    fn start_queued_commit(
        &mut self,
        resource: &wl_surface::WlSurface,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
    ) {
        let next = shared
            .lock()
            .ok()
            .and_then(|mut surface| surface.commit.queued.take());
        if let Some(next) = next {
            self.run_commit(resource, shared, next);
        }
    }

    fn take_in_flight(
        &mut self,
        job: JobId,
    ) -> Option<(Arc<Mutex<WaylandSurfaceState>>, InFlightCommit)> {
        for surface in &self.surfaces {
            let Ok(mut state) = surface.shared.lock() else {
                continue;
            };
            if state
                .commit
                .in_flight
                .as_ref()
                .is_some_and(|flight| flight.job == job)
            {
                let flight = state.commit.in_flight.take()?;
                return Some((Arc::clone(&surface.shared), flight));
            }
        }
        None
    }

    /// Drop every commit a retiring surface still owns and cancel its read.
    pub(super) fn abandon_commits(&mut self, surface: &mut WaylandSurfaceState) {
        if let Some(flight) = surface.commit.in_flight.take() {
            self.snapshots.cancel(flight.job);
            if let Some(buffer) = flight.release {
                surface.pending_buffer_releases.push(buffer);
            }
        }
        if let Some(PendingCommit {
            attach: Some(Some(attachment)),
            ..
        }) = surface.commit.queued.take()
        {
            surface.pending_buffer_releases.push(attachment.resource);
        }
    }

    /// One UI-thread turn of snapshot bookkeeping: apply finished reads and
    /// fail the commits whose read outlived its budget.
    pub(super) fn pump_snapshots(&mut self) {
        for completion in self.snapshots.take_completions() {
            self.complete_commit(completion);
        }
        for expired in self.snapshots.reap_expired() {
            let Some((_, flight)) = self.take_in_flight(expired) else {
                continue;
            };
            flight.resource.post_error(
                wl_shm::Error::InvalidFd,
                "wl_surface.commit: shm snapshot exceeded its time budget".to_string(),
            );
        }
    }

    pub(super) fn snapshots_need_service(&self) -> bool {
        self.snapshots.has_completions() || self.snapshots.expiry_due()
    }
}

enum Planned {
    /// Satisfied without a read.
    Done,
    /// Read this on a snapshot worker.
    InFlight(SnapshotSpec),
    /// A protocol error was posted; the commit is abandoned.
    Abort,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: usize, y: usize, width: usize, height: usize) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn coalescing_unions_damage_and_keeps_the_newest_attachment() {
        let old = PendingCommit {
            attach: None,
            damage: rect(0, 0, 10, 10),
            viewport: ViewportState::default(),
            mapping: BufferMapping::default(),
            decoration_server: true,
            offset: (0, 0),
        };
        let new = PendingCommit {
            attach: Some(None),
            damage: rect(20, 20, 5, 5),
            viewport: ViewportState {
                destination: Some((7, 8)),
                ..Default::default()
            },
            mapping: BufferMapping::default(),
            decoration_server: true,
            offset: (0, 0),
        };
        let (merged, released) = merge_commits(old, new);
        assert!(released.is_none());
        assert!(matches!(merged.attach, Some(None)));
        assert_eq!(merged.damage, rect(0, 0, 25, 25));
        assert_eq!(merged.viewport.destination, Some((7, 8)));
    }

    #[test]
    fn a_later_damage_only_commit_keeps_the_queued_attachment() {
        let old = PendingCommit {
            attach: Some(None),
            damage: Rect::empty(),
            viewport: ViewportState::default(),
            mapping: BufferMapping::default(),
            decoration_server: true,
            offset: (0, 0),
        };
        let new = PendingCommit {
            attach: None,
            damage: rect(1, 1, 2, 2),
            viewport: ViewportState::default(),
            mapping: BufferMapping::default(),
            decoration_server: true,
            offset: (0, 0),
        };
        let (merged, _) = merge_commits(old, new);
        assert!(
            matches!(merged.attach, Some(None)),
            "null attach must survive"
        );
        assert_eq!(merged.damage, rect(1, 1, 2, 2));
    }
}
