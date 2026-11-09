//! Retained wl_shm buffer copies and pixel conversion.

use super::*;

/// How a damaged commit is satisfied from its buffer.
pub(super) enum DamagePlan {
    /// The damage lies outside the buffer: nothing to read, still a commit.
    Empty,
    /// Read exactly the damaged rows.
    Read(SnapshotSpec),
    /// The retained pixels cannot take a patch (first buffer or a layout
    /// change); the caller must plan a full snapshot instead.
    NeedsFull,
}

impl BufferData {
    /// Plan a damage-only snapshot. Planning takes the pool lock only long
    /// enough to clone the descriptor handle: the read itself runs on a
    /// snapshot worker and never under this lock, so a slow client fd cannot
    /// block `create_buffer`/`resize` on the UI thread.
    pub(super) fn plan_damage(
        &self,
        dst_width: usize,
        dst_height: usize,
        dst_stride_pixels: usize,
        dst_len: usize,
        damage: Rect,
    ) -> DamagePlan {
        if dst_width != self.shared.width
            || dst_height != self.shared.height
            || dst_stride_pixels != self.shared.width
            || dst_width.checked_mul(dst_height) != Some(dst_len)
            || dst_len == 0
        {
            return DamagePlan::NeedsFull;
        }
        let damage = damage.intersect(Rect {
            x: 0,
            y: 0,
            width: self.shared.width,
            height: self.shared.height,
        });
        if damage.is_empty() {
            return DamagePlan::Empty;
        }
        let Some((fd, pool_len)) = self.pool_handle() else {
            return DamagePlan::NeedsFull;
        };
        let start = self.shared.offset;
        if validate_wayland_buffer_layout(
            start,
            self.shared.width,
            self.shared.height,
            self.shared.stride,
            pool_len,
        )
        .is_none()
        {
            return DamagePlan::NeedsFull;
        }
        let spec = (|| {
            let width_bytes = damage.width.checked_mul(4)?;
            let read_start = damage
                .y
                .checked_mul(self.shared.stride)
                .and_then(|row| start.checked_add(row))
                .and_then(|pos| pos.checked_add(damage.x.checked_mul(4)?))?;
            let read_len = damage
                .height
                .checked_sub(1)?
                .checked_mul(self.shared.stride)?
                .checked_add(width_bytes)?;
            Some(SnapshotSpec {
                fd,
                read_start,
                read_len,
                layout: SnapshotLayout::Damage {
                    damage,
                    stride: self.shared.stride,
                    has_alpha: self.shared.has_alpha,
                },
            })
        })();
        spec.map_or(DamagePlan::NeedsFull, DamagePlan::Read)
    }

    /// Plan a complete snapshot of this buffer, or `None` when its layout no
    /// longer fits the pool (for example after the pool backing shrank).
    pub(super) fn plan_full(&self) -> Option<SnapshotSpec> {
        let (fd, pool_len) = self.pool_handle()?;
        checked_wayland_pixel_count(self.shared.width, self.shared.height)?;
        let required = validate_wayland_buffer_layout(
            self.shared.offset,
            self.shared.width,
            self.shared.height,
            self.shared.stride,
            pool_len,
        )?;
        if required > MAX_WAYLAND_SHM_POOL_BYTES {
            return None;
        }
        Some(SnapshotSpec {
            fd,
            read_start: self.shared.offset,
            read_len: required,
            layout: SnapshotLayout::Full {
                width: self.shared.width,
                height: self.shared.height,
                stride: self.shared.stride,
                has_alpha: self.shared.has_alpha,
            },
        })
    }

    fn pool_handle(&self) -> Option<(Arc<OwnedFd>, usize)> {
        let pool = self.shared.pool.lock().ok()?;
        Some((Arc::clone(&pool.fd), pool.len))
    }
}

pub(super) fn can_retain_undamaged_buffer(
    has_snapshot: bool,
    current_width: usize,
    current_height: usize,
    current_stride_pixels: usize,
    next_width: usize,
    next_height: usize,
    next_stride_pixels: usize,
) -> bool {
    has_snapshot
        && current_width == next_width
        && current_height == next_height
        && current_stride_pixels == next_stride_pixels
}

pub(super) fn next_content_version(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
}
