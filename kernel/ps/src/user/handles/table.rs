use super::*;
use alloc::vec::Vec;

#[derive(Debug, Clone)]
pub struct HandleEntry {
    handle: KernelHandle,
    token: HandleToken,
    rights: HandleRights,
    fd_flags: u32,
    status_flags: u64,
}

impl HandleEntry {
    pub fn new(handle: KernelHandle, fd_flags: u32, status_flags: u64) -> Self {
        let status_flags = status_flags & STATUS_FLAG_MASK;
        let rights = handle.default_rights(status_flags);
        Self::new_with_rights(handle, rights, fd_flags, status_flags)
    }

    pub fn new_with_rights(
        handle: KernelHandle,
        rights: HandleRights,
        fd_flags: u32,
        status_flags: u64,
    ) -> Self {
        Self {
            token: handle.token(),
            handle,
            rights,
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

    pub fn rights(&self) -> HandleRights {
        self.rights
    }

    pub fn supports_transfer(&self) -> bool {
        self.handle.supports_descriptor_transfer(self.rights)
    }

    pub fn ipc_transfer_descriptor(
        &self,
        transfer_id: u64,
    ) -> Option<crate::ipc::KernelTransferredHandle> {
        (transfer_id != 0 && self.supports_transfer())
            .then(|| crate::ipc::KernelTransferredHandle::new(transfer_id, self.token, self.rights))
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

#[derive(Debug, Clone)]
pub struct TransferredHandleEntry {
    entry: HandleEntry,
}

impl TransferredHandleEntry {
    pub fn from_entry(entry: HandleEntry) -> Option<Self> {
        entry.supports_transfer().then_some(Self { entry })
    }

    pub fn entry(&self) -> &HandleEntry {
        &self.entry
    }

    pub fn ipc_descriptor(&self, transfer_id: u64) -> Option<crate::ipc::KernelTransferredHandle> {
        self.entry.ipc_transfer_descriptor(transfer_id)
    }

    pub fn into_entry(self) -> HandleEntry {
        self.entry
    }
}

pub struct HandleTable {
    entries: Vec<Option<HandleEntry>>,
}

impl Clone for HandleTable {
    fn clone(&self) -> Self {
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

    pub fn install(&mut self, handle: KernelHandle) -> Option<u64> {
        self.install_with_open_flags(handle, 0)
    }

    pub fn install_with_open_flags(
        &mut self,
        handle: KernelHandle,
        open_flags: u64,
    ) -> Option<u64> {
        let fd_flags = if open_flags & linux_abi::O_CLOEXEC != 0 {
            FD_CLOEXEC
        } else {
            0
        };
        let status_flags = open_flags & STATUS_FLAG_MASK;
        self.install_entry(HandleEntry::new(handle, fd_flags, status_flags))
    }

    pub fn install_with_open_flags_and_rights(
        &mut self,
        handle: KernelHandle,
        open_flags: u64,
        rights: HandleRights,
    ) -> Option<u64> {
        let fd_flags = if open_flags & linux_abi::O_CLOEXEC != 0 {
            FD_CLOEXEC
        } else {
            0
        };
        let status_flags = open_flags & STATUS_FLAG_MASK;
        self.install_entry(HandleEntry::new_with_rights(
            handle,
            rights,
            fd_flags,
            status_flags,
        ))
    }

    pub fn install_entry(&mut self, entry: HandleEntry) -> Option<u64> {
        self.install_entry_min(entry, FIRST_DYNAMIC_FD as u64)
    }

    pub fn install_entry_min(&mut self, entry: HandleEntry, min_fd: u64) -> Option<u64> {
        if min_fd > MAX_DYNAMIC_FD {
            return None;
        }
        let start_index = dynamic_index(min_fd.max(FIRST_DYNAMIC_FD as u64))?;
        if let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .skip(start_index)
            .find_map(|(index, entry)| entry.is_none().then_some(index))
        {
            self.entries[index] = Some(entry);
            return Some(FIRST_DYNAMIC_FD as u64 + index as u64);
        }

        if self.entries.len() >= max_dynamic_entries() {
            return None;
        }
        if self.entries.len() < start_index {
            self.entries.resize_with(start_index, || None);
        }
        self.entries.push(Some(entry));
        Some(FIRST_DYNAMIC_FD as u64 + (self.entries.len() - 1) as u64)
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

    pub fn duplicate_for_transfer(&self, fd: u64) -> Option<TransferredHandleEntry> {
        TransferredHandleEntry::from_entry(self.get_entry(fd)?.clone())
    }

    pub fn install_transferred(&mut self, transferred: TransferredHandleEntry) -> Option<u64> {
        self.install_entry(transferred.into_entry())
    }

    pub fn install_transferred_min(
        &mut self,
        transferred: TransferredHandleEntry,
        min_fd: u64,
    ) -> Option<u64> {
        self.install_entry_min(transferred.into_entry(), min_fd)
    }

    pub fn can_install_additional(&self, count: usize) -> bool {
        let reusable = self.entries.iter().filter(|entry| entry.is_none()).count();
        let appendable = max_dynamic_entries().saturating_sub(self.entries.len());
        count <= reusable.saturating_add(appendable)
    }

    pub fn display_surface_count(&self) -> usize {
        self.entries
            .iter()
            .flatten()
            .filter(|entry| matches!(entry.handle(), KernelHandle::DisplaySurface(_)))
            .count()
    }

    pub fn gpu_atlas_slot_in_use(&self, slot: u32) -> bool {
        self.entries.iter().flatten().any(|entry| {
            matches!(
                entry.handle(),
                KernelHandle::DisplaySurface(surface)
                    if surface.binding_slot() == Some(slot)
            )
        })
    }

    fn ensure_entry_capacity(&mut self, index: usize) -> Option<()> {
        let required = index.checked_add(1)?;
        if required > max_dynamic_entries() {
            return None;
        }
        if self.entries.len() < required {
            self.entries.resize_with(required, || None);
        }
        Some(())
    }

    pub fn replace_entry(&mut self, fd: u64, entry: Option<HandleEntry>) -> Option<()> {
        if fd > MAX_DYNAMIC_FD {
            return None;
        }
        let index = dynamic_index(fd)?;
        self.ensure_entry_capacity(index)?;
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

            let _ = entry.take();
        }
    }

    pub fn duplicate_min(&mut self, fd: u64, min_fd: u64, close_on_exec: bool) -> Option<u64> {
        if min_fd > MAX_DYNAMIC_FD {
            return None;
        }
        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        self.install_entry_min(entry, min_fd)
    }

    pub fn duplicate_exact(&mut self, fd: u64, new_fd: u64, close_on_exec: bool) -> Option<u64> {
        if !(FIRST_DYNAMIC_FD as u64..=MAX_DYNAMIC_FD).contains(&new_fd) {
            return None;
        }

        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        let index = dynamic_index(new_fd)?;
        self.ensure_entry_capacity(index)?;
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

fn dynamic_index(fd: u64) -> Option<usize> {
    fd.checked_sub(FIRST_DYNAMIC_FD as u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn max_dynamic_entries() -> usize {
    usize::try_from(MAX_DYNAMIC_FD - FIRST_DYNAMIC_FD as u64 + 1)
        .expect("dynamic descriptor ceiling must fit usize")
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{
        FD_CLOEXEC, HandleEntry, HandleTable, KernelHandle, MAX_DYNAMIC_FD, VfsDirectoryHandle,
        max_dynamic_entries,
    };
    use crate::memory::paging::UserRegion;
    use crate::user::linux as linux_abi;
    use kernel_object::api::handle::{FileHandleRights, HandleOwner, HandleRights};
    use x86_64::VirtAddr;

    #[test]
    fn install_entry_min_keeps_existing_dynamic_fds() {
        let mut table = HandleTable::new();

        let fd0 = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/a".into(),
            vec![],
        )));
        let fd1 = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/b".into(),
            vec![],
        )));
        let fd2 = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/c".into(),
            vec![],
        )));

        assert_eq!(fd0, Some(3));
        assert_eq!(fd1, Some(4));
        assert_eq!(fd2, Some(5));
    }

    #[test]
    fn close_cloexec_removes_only_flagged_entries() {
        let mut table = HandleTable::new();

        let keep_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/keep".into(), vec![])),
            0,
            0,
        ));
        let drop_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/drop".into(), vec![])),
            FD_CLOEXEC,
            0,
        ));

        table.close_cloexec();

        assert!(table.get(keep_fd.expect("keep descriptor")).is_some());
        assert!(table.get(drop_fd.expect("cloexec descriptor")).is_none());
    }

    #[test]
    fn set_status_flags_preserves_access_mode_and_masks_unknown_bits() {
        let mut entry = HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/flags".into(), vec![])),
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
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
            0,
            linux_abi::O_RDONLY,
        ));
        let target_fd = table.install_entry(HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/target".into(), vec![])),
            0,
            linux_abi::O_RDONLY,
        ));

        assert_eq!(
            table.duplicate_exact(
                source_fd.expect("source descriptor"),
                target_fd.expect("target descriptor"),
                true,
            ),
            target_fd
        );
        let replaced = table
            .get_entry(target_fd.expect("target descriptor"))
            .expect("duplicated entry");
        assert_eq!(replaced.fd_flags() & FD_CLOEXEC, FD_CLOEXEC);
        match replaced.handle() {
            KernelHandle::VfsDirectory(dir) => assert_eq!(dir.path(), "/source"),
            other => panic!("expected VfsDirectory after dup2-style replace, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_exact_preserves_handle_rights() {
        let mut table = HandleTable::new();
        let rights = HandleRights::File(FileHandleRights::READ);
        let source_fd = table.install_entry(HandleEntry::new_with_rights(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
            rights,
            0,
            linux_abi::O_RDONLY,
        ));

        let target_fd = table
            .duplicate_exact(source_fd.expect("source descriptor"), 10, false)
            .expect("dup");

        assert_eq!(table.get_entry(target_fd).expect("target").rights(), rights);
    }

    #[test]
    fn duplication_rejects_sparse_descriptor_indices_above_the_ceiling() {
        let mut table = HandleTable::new();
        let source_fd = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/source".into(),
            vec![],
        )));

        assert_eq!(
            table.duplicate_exact(
                source_fd.expect("source descriptor"),
                MAX_DYNAMIC_FD + 1,
                false,
            ),
            None
        );
        assert_eq!(
            table.duplicate_min(
                source_fd.expect("source descriptor"),
                MAX_DYNAMIC_FD + 1,
                false,
            ),
            None
        );
        assert_eq!(table.entries.len(), 1);
    }

    #[test]
    fn transfer_duplicate_requires_transfer_right() {
        let mut table = HandleTable::new();
        let source_fd = table.install_entry(HandleEntry::new_with_rights(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
            HandleRights::File(FileHandleRights::READ),
            0,
            linux_abi::O_RDONLY,
        ));

        assert!(
            table
                .duplicate_for_transfer(source_fd.expect("source descriptor"))
                .is_none()
        );
    }

    #[test]
    fn transfer_install_preserves_rights_and_flags() {
        let mut source = HandleTable::new();
        let source_fd = source.install_entry(HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/source".into(), vec![])),
            FD_CLOEXEC,
            linux_abi::O_RDONLY | linux_abi::O_NONBLOCK,
        ));
        let transferred = source
            .duplicate_for_transfer(source_fd.expect("source descriptor"))
            .expect("transferable source fd");
        assert!(transferred.ipc_descriptor(0).is_none());
        let descriptor = transferred
            .ipc_descriptor(99)
            .expect("ipc transfer descriptor");
        assert_eq!(descriptor.transfer_id(), 99);
        assert_eq!(descriptor.token().owner(), HandleOwner::Io);
        assert!(descriptor.rights().allows_transfer());

        let mut target = HandleTable::new();
        let target_fd = target
            .install_transferred(transferred)
            .expect("target descriptor");
        let target_entry = target.get_entry(target_fd).expect("target fd");
        assert_eq!(target_entry.fd_flags() & FD_CLOEXEC, FD_CLOEXEC);
        assert_ne!(target_entry.status_flags() & linux_abi::O_NONBLOCK, 0);
        assert!(target_entry.rights().allows_transfer());
    }

    #[test]
    fn directory_fds_are_file_caps_for_vfs_transfer() {
        let mut table = HandleTable::new();
        let dir_fd = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/dir".into(),
            vec![],
        )));

        let transferred = table
            .duplicate_for_transfer(dir_fd.expect("directory descriptor"))
            .expect("directory fd should be transferable");
        assert!(transferred.entry().rights().allows_transfer());
    }

    #[test]
    fn device_fds_are_transferable_for_policy_brokers() {
        let mut table = HandleTable::new();
        let display_fd = table.install(KernelHandle::Device(
            crate::io::device::DeviceHandle::with_access(
                crate::io::device::DeviceId::Display,
                crate::io::device::DeviceAccessKind::Native,
            ),
        ));

        let transferred = table
            .duplicate_for_transfer(display_fd.expect("device descriptor"))
            .expect("device fd should be transferable after policy approval");
        assert!(transferred.entry().rights().allows_transfer());
    }

    #[test]
    fn display_surface_count_ignores_other_handle_kinds() {
        let mut table = HandleTable::new();
        assert_eq!(table.display_surface_count(), 0);

        table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/file".into(),
            vec![],
        )));
        assert_eq!(table.display_surface_count(), 0);

        let surface = super::DisplaySurfaceHandle::new(
            16,
            16,
            crate::user::abi::device::PIXEL_FORMAT_BGRA8888,
            1,
        )
        .expect("surface");
        table.install(KernelHandle::DisplaySurface(surface));
        assert_eq!(table.display_surface_count(), 1);
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

    #[test]
    fn dynamic_install_never_exceeds_descriptor_ceiling() {
        let mut table = HandleTable::new();
        let occupied = HandleEntry::new(
            KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/occupied".into(), vec![])),
            0,
            linux_abi::O_RDONLY,
        );
        table.entries.resize(max_dynamic_entries(), Some(occupied));
        table.entries[max_dynamic_entries() - 1] = None;

        let last = table.install_entry_min(
            HandleEntry::new(
                KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/last".into(), vec![])),
                0,
                linux_abi::O_RDONLY,
            ),
            MAX_DYNAMIC_FD,
        );
        assert_eq!(last, Some(MAX_DYNAMIC_FD));
        assert_eq!(table.entries.len(), max_dynamic_entries());
        assert!(!table.can_install_additional(1));

        let rejected = table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
            "/overflow".into(),
            vec![],
        )));
        assert_eq!(rejected, None);
        assert_eq!(table.entries.len(), max_dynamic_entries());
    }
}
