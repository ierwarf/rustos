//! Retained wl_shm buffer copies and pixel conversion.

use super::*;

impl BufferData {
    pub(super) fn copy_damage_into(
        &self,
        dst: &mut Arc<Vec<u32>>,
        dst_width: usize,
        dst_height: usize,
        dst_stride_pixels: usize,
        damage: Rect,
    ) -> bool {
        if dst_width != self.shared.width
            || dst_height != self.shared.height
            || dst_stride_pixels != self.shared.width
            || dst_width
                .checked_mul(dst_height)
                .is_none_or(|len| len != dst.len())
            || dst.is_empty()
        {
            return false;
        }
        let damage = damage.intersect(Rect {
            x: 0,
            y: 0,
            width: self.shared.width,
            height: self.shared.height,
        });
        if damage.is_empty() {
            return true;
        }
        let Ok(pool) = self.shared.pool.lock() else {
            return false;
        };
        let start = self.shared.offset;
        if validate_wayland_buffer_layout(
            start,
            self.shared.width,
            self.shared.height,
            self.shared.stride,
            pool.len,
        )
        .is_none()
        {
            return false;
        }
        let Some(width_bytes) = damage.width.checked_mul(4) else {
            return false;
        };
        // Read into owned bytes before modifying retained pixels. A concurrent
        // truncate can only cause short I/O, never SIGBUS or a partial update.
        let Some(read_start) = damage
            .y
            .checked_mul(self.shared.stride)
            .and_then(|row| start.checked_add(row))
            .and_then(|pos| pos.checked_add(damage.x.checked_mul(4)?))
        else {
            return false;
        };
        let Some(read_len) = damage
            .height
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(self.shared.stride))
            .and_then(|prefix| prefix.checked_add(width_bytes))
        else {
            return false;
        };
        let Ok(bytes) = super::shm::read_shm_snapshot(pool.fd.as_raw_fd(), read_start, read_len)
        else {
            return false;
        };
        let pixels = Arc::make_mut(dst);
        for row in 0..damage.height {
            let Some(src_row) = row.checked_mul(self.shared.stride) else {
                return false;
            };
            let Some(src_end) = src_row.checked_add(width_bytes) else {
                return false;
            };
            let Some(row_bytes) = bytes.get(src_row..src_end) else {
                return false;
            };
            let Some(dst_row) = (damage.y + row)
                .checked_mul(dst_stride_pixels)
                .and_then(|row_start| row_start.checked_add(damage.x))
            else {
                return false;
            };
            let Some(dst_end) = dst_row.checked_add(damage.width) else {
                return false;
            };
            let Some(row_pixels) = pixels.get_mut(dst_row..dst_end) else {
                return false;
            };
            if !copy_wayland_bgra_row(row_bytes, row_pixels, self.shared.has_alpha) {
                return false;
            }
        }
        true
    }

    pub(super) fn copy_pixels(&self) -> Option<(Arc<Vec<u32>>, usize, usize, usize)> {
        let pool = self.shared.pool.lock().ok()?;
        let start = self.shared.offset;
        let pixel_count = checked_wayland_pixel_count(self.shared.width, self.shared.height)?;
        let width_bytes = self.shared.width.checked_mul(4)?;
        let required = validate_wayland_buffer_layout(
            start,
            self.shared.width,
            self.shared.height,
            self.shared.stride,
            pool.len,
        )?;
        if required > MAX_WAYLAND_SHM_POOL_BYTES {
            return None;
        }

        let bytes = super::shm::read_shm_snapshot(pool.fd.as_raw_fd(), start, required).ok()?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(pixel_count).ok()?;
        pixels.resize(pixel_count, 0);
        for row in 0..self.shared.height {
            let row_start = row.checked_mul(self.shared.stride)?;
            let row_end = row_start.checked_add(width_bytes)?;
            let row_bytes = bytes.get(row_start..row_end)?;
            let dst_start = row.checked_mul(self.shared.width)?;
            let dst_end = dst_start.checked_add(self.shared.width)?;
            if !copy_wayland_bgra_row(
                row_bytes,
                pixels.get_mut(dst_start..dst_end)?,
                self.shared.has_alpha,
            ) {
                return None;
            }
        }
        Some((
            Arc::new(pixels),
            self.shared.width,
            self.shared.height,
            self.shared.width,
        ))
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
