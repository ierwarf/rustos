use super::*;
use alloc::vec::Vec;

#[derive(Debug, Clone)]
pub struct HandleEntry {
    handle: KernelHandle,
    token: HandleToken,
    fd_flags: u32,
    status_flags: u64,
}

impl HandleEntry {
    pub fn new(handle: KernelHandle, fd_flags: u32, status_flags: u64) -> Self {
        Self {
            token: handle.token(),
            handle,
            fd_flags,
            status_flags: status_flags & STATUS_FLAG_MASK,
        }
    }

    pub fn handle(&self) -> &KernelHandle {
        &self.handle
    }

    pub fn handle_mut(&mut self) -> &mut KernelHandle {
        &mut self.handle
    }

    pub fn token(&self) -> HandleToken {
        self.token
    }

    pub fn into_handle(self) -> KernelHandle {
        self.handle
    }

    pub fn fd_flags(&self) -> u32 {
        self.fd_flags
    }

    pub fn set_fd_flags(&mut self, fd_flags: u32) {
        self.fd_flags = fd_flags & FD_CLOEXEC;
    }

    pub fn status_flags(&self) -> u64 {
        self.status_flags
    }

    pub fn set_status_flags(&mut self, status_flags: u64) {
        let access_mode = self.status_flags & linux_abi::O_ACCMODE;
        let mutable = status_flags & !linux_abi::O_ACCMODE;
        self.status_flags = access_mode | (mutable & STATUS_FLAG_MASK);
    }
}

pub struct HandleTable {
    entries: Vec<Option<HandleEntry>>,
}

impl Clone for HandleTable {
    fn clone(&self) -> Self {
        for entry in self.entries.iter().flatten() {
            on_handle_open(entry.handle());
        }
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn install(&mut self, handle: KernelHandle) -> u64 {
        self.install_with_open_flags(handle, 0)
    }

    pub fn install_with_open_flags(&mut self, handle: KernelHandle, open_flags: u64) -> u64 {
        let fd_flags = if open_flags & linux_abi::O_CLOEXEC != 0 {
            FD_CLOEXEC
        } else {
            0
        };
        let status_flags = open_flags & STATUS_FLAG_MASK;
        self.install_entry(HandleEntry::new(handle, fd_flags, status_flags))
    }

    pub fn install_entry(&mut self, entry: HandleEntry) -> u64 {
        self.install_entry_min(entry, FIRST_DYNAMIC_FD as u64)
    }

    pub fn install_entry_min(&mut self, entry: HandleEntry, min_fd: u64) -> u64 {
        let start_index = dynamic_index(min_fd.max(FIRST_DYNAMIC_FD as u64)).unwrap_or(0);
        if let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .skip(start_index)
            .find_map(|(index, entry)| entry.is_none().then_some(index))
        {
            on_handle_open(entry.handle());
            self.entries[index] = Some(entry);
            return FIRST_DYNAMIC_FD as u64 + index as u64;
        }

        if self.entries.len() < start_index {
            self.entries.resize_with(start_index, || None);
        }
        on_handle_open(entry.handle());
        self.entries.push(Some(entry));
        FIRST_DYNAMIC_FD as u64 + (self.entries.len() - 1) as u64
    }

    pub fn get(&self, fd: u64) -> Option<&KernelHandle> {
        Some(self.get_entry(fd)?.handle())
    }

    pub fn get_mut(&mut self, fd: u64) -> Option<&mut KernelHandle> {
        Some(self.get_entry_mut(fd)?.handle_mut())
    }

    pub fn get_entry(&self, fd: u64) -> Option<&HandleEntry> {
        let index = dynamic_index(fd)?;
        self.entries.get(index)?.as_ref()
    }

    pub fn get_entry_mut(&mut self, fd: u64) -> Option<&mut HandleEntry> {
        let index = dynamic_index(fd)?;
        self.entries.get_mut(index)?.as_mut()
    }

    pub fn ensure_entry_capacity(&mut self, index: usize) {
        if self.entries.len() <= index {
            self.entries.resize_with(index + 1, || None);
        }
    }

    pub fn replace_entry(&mut self, fd: u64, entry: Option<HandleEntry>) -> Option<()> {
        let index = dynamic_index(fd)?;
        self.ensure_entry_capacity(index);
        if let Some(existing) = self.entries[index].as_ref() {
            on_handle_close(existing.handle());
        }
        if let Some(new_entry) = entry.as_ref() {
            on_handle_open(new_entry.handle());
        }
        self.entries[index] = entry;
        Some(())
    }

    pub fn close(&mut self, fd: u64) -> Option<KernelHandle> {
        let index = dynamic_index(fd)?;
        let handle = self
            .entries
            .get_mut(index)?
            .take()
            .map(HandleEntry::into_handle)?;
        on_handle_close(&handle);
        Some(handle)
    }

    pub fn close_cloexec(&mut self) {
        for entry in &mut self.entries {
            let Some(current) = entry.as_ref() else {
                continue;
            };
            if current.fd_flags() & FD_CLOEXEC == 0 {
                continue;
            }

            let handle = entry
                .take()
                .expect("close-on-exec entry must exist")
                .into_handle();
            on_handle_close(&handle);
        }
    }

    pub fn duplicate_min(
        &mut self,
        fd: u64,
        min_fd: u64,
        close_on_exec: bool,
    ) -> Option<u64> {
        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        Some(self.install_entry_min(entry, min_fd))
    }

    pub fn duplicate_exact(
        &mut self,
        fd: u64,
        new_fd: u64,
        close_on_exec: bool,
    ) -> Option<u64> {
        if new_fd < FIRST_DYNAMIC_FD as u64 {
            return None;
        }

        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        let index = dynamic_index(new_fd)?;
        self.ensure_entry_capacity(index);
        if let Some(existing) = self.entries[index].as_ref() {
            on_handle_close(existing.handle());
        }
        on_handle_open(entry.handle());
        self.entries[index] = Some(entry);
        Some(new_fd)
    }

    pub fn clear_surface_mappings_in_range(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len) else {
            return;
        };

        for entry in &mut self.entries {
            let Some(entry) = entry.as_mut() else {
                continue;
            };
            let KernelHandle::DisplaySurface(surface) = entry.handle_mut() else {
                continue;
            };
            let Some(region) = surface.mapped_region() else {
                continue;
            };
            let region_start = region.start.as_u64();
            let region_end = region.end().as_u64();
            if start < region_end && end > region_start {
                surface.clear_mapping();
            }
        }
    }

    pub fn surface_overlap_segments(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut segments = Vec::new();
        if start >= end {
            return segments;
        }

        for entry in &self.entries {
            let Some(entry) = entry.as_ref() else {
                continue;
            };
            let KernelHandle::DisplaySurface(surface) = entry.handle() else {
                continue;
            };
            let Some(region) = surface.mapped_region() else {
                continue;
            };
            let region_start = region.start.as_u64();
            let region_end = region.end().as_u64();
            let overlap_start = start.max(region_start);
            let overlap_end = end.min(region_end);
            if overlap_start < overlap_end {
                segments.push((overlap_start, overlap_end - overlap_start));
            }
        }

        segments
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HandleTable {
    fn drop(&mut self) {
        for entry in self.entries.iter().flatten() {
            on_handle_close(entry.handle());
        }
    }
}

fn dynamic_index(fd: u64) -> Option<usize> {
    fd.checked_sub(FIRST_DYNAMIC_FD as u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn on_handle_open(handle: &KernelHandle) {
    if let KernelHandle::Device(device) = handle {
        if device.device_id() == crate::io::device::DeviceId::Input
            && device.access_kind() == crate::io::device::DeviceAccessKind::Native
        {
            crate::driver::linux::input::consumer_acquire();
        }
    }
}

fn on_handle_close(handle: &KernelHandle) {
    if let KernelHandle::Device(device) = handle {
        if device.device_id() == crate::io::device::DeviceId::Input
            && device.access_kind() == crate::io::device::DeviceAccessKind::Native
        {
            crate::driver::linux::input::consumer_release();
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{FD_CLOEXEC, HandleEntry, HandleTable, KernelHandle, VfsFileHandle};
    use crate::memory::paging::UserRegion;
    use crate::user::linux as linux_abi;
    use x86_64::VirtAddr;

    #[test]
    fn install_entry_min_keeps_existing_dynamic_fds() {
        let mut table = HandleTable::new();

        let fd0 = table.install(KernelHandle::VfsFile(VfsFileHandle::read_only_memory(
            "/a".into(),
            vec![1],
        )));
        let fd1 = table.install(KernelHandle::VfsFile(VfsFileHandle::read_only_memory(
            "/b".into(),
            vec![2],
        )));
        let fd2 = table.install(KernelHandle::VfsFile(VfsFileHandle::read_only_memory(
            "/c".into(),
            vec![3],
        )));

        assert_eq!(fd0, 3);
        assert_eq!(fd1, 4);
        assert_eq!(fd2, 5);
    }

    #[test]
    fn close_cloexec_removes_only_flagged_entries() {
        let mut table = HandleTable::new();

        let keep_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/keep".into(), vec![1])),
            0,
            0,
        ));
        let drop_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/drop".into(), vec![2])),
            FD_CLOEXEC,
            0,
        ));

        table.close_cloexec();

        assert!(table.get(keep_fd).is_some());
        assert!(table.get(drop_fd).is_none());
    }

    #[test]
    fn set_status_flags_preserves_access_mode_and_masks_unknown_bits() {
        let mut entry = HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/flags".into(), vec![1])),
            0,
            linux_abi::O_RDWR | linux_abi::O_APPEND,
        );

        entry.set_status_flags(linux_abi::O_RDONLY | linux_abi::O_NONBLOCK | (1_u64 << 63));
        assert_eq!(
            entry.status_flags() & linux_abi::O_ACCMODE,
            linux_abi::O_RDWR
        );
        assert_ne!(entry.status_flags() & linux_abi::O_NONBLOCK, 0);
        assert_eq!(entry.status_flags() & linux_abi::O_APPEND, 0);
        assert_eq!(entry.status_flags() & (1_u64 << 63), 0);
    }

    #[test]
    fn duplicate_exact_replaces_target_and_applies_cloexec_flag() {
        let mut table = HandleTable::new();
        let source_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/source".into(), vec![1])),
            0,
            linux_abi::O_RDONLY,
        ));
        let target_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory("/target".into(), vec![2])),
            0,
            linux_abi::O_RDONLY,
        ));

        assert_eq!(
            table.duplicate_exact(source_fd, target_fd, true),
            Some(target_fd)
        );
        let replaced = table.get_entry(target_fd).expect("duplicated entry");
        assert_eq!(replaced.fd_flags() & FD_CLOEXEC, FD_CLOEXEC);
        match replaced.handle() {
            KernelHandle::VfsFile(file) => assert_eq!(file.path(), "/source"),
            other => panic!("expected VfsFile after dup2-style replace, got {other:?}"),
        }
    }

    #[test]
    fn surface_overlap_segments_return_intersection_ranges() {
        let mut table = HandleTable::new();
        let mut surface = super::DisplaySurfaceHandle::new(
            1280,
            800,
            crate::user::abi::device::PIXEL_FORMAT_BGRA8888,
            1,
        )
        .expect("surface");
        surface.set_mapped_region(UserRegion {
            start: VirtAddr::new(0x4000_0000),
            page_count: 4,
        });
        table.install(KernelHandle::DisplaySurface(surface));

        let segments = table.surface_overlap_segments(0x4000_1000, 0x4000_5000);
        assert_eq!(segments, vec![(0x4000_1000, 0x3000)]);

        let disjoint = table.surface_overlap_segments(0x5000_0000, 0x5000_1000);
        assert!(disjoint.is_empty());
    }
}
