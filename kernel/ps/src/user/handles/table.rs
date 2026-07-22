use super::*;
use alloc::collections::BTreeMap;
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

#[derive(Debug)]
pub struct TransferredHandleEntry {
    entry: Option<HandleEntry>,
}

impl TransferredHandleEntry {
    pub fn from_entry(entry: HandleEntry) -> Option<Self> {
        if !entry.supports_transfer() {
            return None;
        }
        if let KernelHandle::Console(console) = entry.handle() {
            console.acquire_descriptor_reference();
        }
        Some(Self { entry: Some(entry) })
    }

    pub fn entry(&self) -> &HandleEntry {
        self.entry
            .as_ref()
            .expect("transferred handle entry was already consumed")
    }

    pub fn ipc_descriptor(&self, transfer_id: u64) -> Option<crate::ipc::KernelTransferredHandle> {
        self.entry().ipc_transfer_descriptor(transfer_id)
    }

    pub fn into_entry(self) -> HandleEntry {
        let mut this = self;
        this.entry
            .take()
            .expect("transferred handle entry was already consumed")
    }
}

impl Drop for TransferredHandleEntry {
    fn drop(&mut self) {
        release_console_entry_reference(self.entry.as_ref());
    }
}

pub struct HandleTable {
    standard: [Option<HandleEntry>; FIRST_DYNAMIC_FD as usize],
    entries: Vec<Option<HandleEntry>>,
    reserved: BTreeMap<u64, u64>,
    next_reservation_id: u64,
}

impl Clone for HandleTable {
    fn clone(&self) -> Self {
        let cloned = Self {
            standard: self.standard.clone(),
            entries: self.entries.clone(),
            // An in-flight receive transaction belongs to the calling
            // process only. Forked children must not inherit invisible slots.
            reserved: BTreeMap::new(),
            next_reservation_id: self.next_reservation_id,
        };
        for entry in cloned
            .standard
            .iter()
            .chain(cloned.entries.iter())
            .flatten()
        {
            if let KernelHandle::Console(console) = entry.handle() {
                console.acquire_descriptor_reference();
            }
        }
        cloned
    }
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            standard: [
                Some(HandleEntry::new(
                    KernelHandle::Console(ConsoleHandle::new(ConsoleStreamKind::Input)),
                    0,
                    linux_abi::O_RDONLY,
                )),
                Some(HandleEntry::new(
                    KernelHandle::Console(ConsoleHandle::new(ConsoleStreamKind::Output)),
                    0,
                    linux_abi::O_WRONLY,
                )),
                Some(HandleEntry::new(
                    KernelHandle::Console(ConsoleHandle::new(ConsoleStreamKind::Error)),
                    0,
                    linux_abi::O_WRONLY,
                )),
            ],
            entries: Vec::new(),
            reserved: BTreeMap::new(),
            next_reservation_id: 1,
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
        self.install_entry_min(entry, 0)
    }

    pub fn install_entry_min(&mut self, entry: HandleEntry, min_fd: u64) -> Option<u64> {
        if min_fd > MAX_DYNAMIC_FD {
            return None;
        }
        let standard_start = usize::try_from(min_fd.min(FIRST_DYNAMIC_FD as u64)).ok()?;
        if let Some(index) = self
            .standard
            .iter()
            .enumerate()
            .skip(standard_start)
            .find_map(|(index, entry)| {
                (entry.is_none() && !self.reserved.contains_key(&(index as u64))).then_some(index)
            })
        {
            self.standard[index] = Some(entry);
            return Some(index as u64);
        }
        let start_index = dynamic_index(min_fd.max(FIRST_DYNAMIC_FD as u64))?;
        if let Some(index) =
            self.entries
                .iter()
                .enumerate()
                .skip(start_index)
                .find_map(|(index, entry)| {
                    let fd = FIRST_DYNAMIC_FD as u64 + index as u64;
                    (entry.is_none() && !self.reserved.contains_key(&fd)).then_some(index)
                })
        {
            self.entries[index] = Some(entry);
            return Some(FIRST_DYNAMIC_FD as u64 + index as u64);
        }

        let mut index = self.entries.len().max(start_index);
        while index < max_dynamic_entries() {
            let fd = FIRST_DYNAMIC_FD as u64 + index as u64;
            if !self.reserved.contains_key(&fd) {
                if self.entries.len() <= index {
                    self.entries.resize_with(index + 1, || None);
                }
                self.entries[index] = Some(entry);
                return Some(fd);
            }
            index = index.checked_add(1)?;
        }
        None
    }

    pub fn get(&self, fd: u64) -> Option<&KernelHandle> {
        Some(self.get_entry(fd)?.handle())
    }

    pub fn get_mut(&mut self, fd: u64) -> Option<&mut KernelHandle> {
        Some(self.get_entry_mut(fd)?.handle_mut())
    }

    pub fn get_entry(&self, fd: u64) -> Option<&HandleEntry> {
        if let Some(index) = standard_index(fd) {
            return self.standard.get(index)?.as_ref();
        }
        let index = dynamic_index(fd)?;
        self.entries.get(index)?.as_ref()
    }

    pub fn get_entry_mut(&mut self, fd: u64) -> Option<&mut HandleEntry> {
        if let Some(index) = standard_index(fd) {
            return self.standard.get_mut(index)?.as_mut();
        }
        let index = dynamic_index(fd)?;
        self.entries.get_mut(index)?.as_mut()
    }

    pub fn is_reserved(&self, fd: u64) -> bool {
        self.reserved.contains_key(&fd)
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
        let occupied = self.standard.iter().filter(|entry| entry.is_some()).count()
            + self.entries.iter().filter(|entry| entry.is_some()).count();
        let total = (FIRST_DYNAMIC_FD as usize).saturating_add(max_dynamic_entries());
        count
            <= total
                .saturating_sub(occupied)
                .saturating_sub(self.reserved.len())
    }

    /// Reserves dynamic descriptor numbers without publishing handles through
    /// `get_entry`. This is the prepare phase for transactional IPC receive.
    pub fn reserve_slots(&mut self, count: usize) -> Option<(u64, Vec<u64>)> {
        let occupied = self.standard.iter().filter(|entry| entry.is_some()).count()
            + self.entries.iter().filter(|entry| entry.is_some()).count();
        let total = (FIRST_DYNAMIC_FD as usize).saturating_add(max_dynamic_entries());
        let free = total
            .saturating_sub(occupied)
            .saturating_sub(self.reserved.len());
        if count > free {
            return None;
        }

        let reservation_id = self.allocate_reservation_id()?;
        let mut slots = Vec::with_capacity(count);
        for fd in 0..=MAX_DYNAMIC_FD {
            if slots.len() == count {
                break;
            }
            let occupied = self.get_entry(fd).is_some();
            if !occupied && self.reserved.insert(fd, reservation_id).is_none() {
                slots.push(fd);
            }
        }
        if slots.len() != count {
            for fd in &slots {
                self.reserved.remove(fd);
            }
            return None;
        }
        Some((reservation_id, slots))
    }

    fn allocate_reservation_id(&mut self) -> Option<u64> {
        for _ in 0..=self.reserved.len() {
            let candidate = self.next_reservation_id;
            self.next_reservation_id = self.next_reservation_id.wrapping_add(1).max(1);
            if candidate != 0 && !self.reserved.values().any(|&id| id == candidate) {
                return Some(candidate);
            }
        }
        None
    }

    pub fn cancel_reservations(&mut self, reservation_id: u64, slots: &[u64]) {
        for &fd in slots {
            if self.reserved.get(&fd) == Some(&reservation_id) {
                self.reserved.remove(&fd);
            }
        }
    }

    /// Atomically publishes every prepared transfer entry. On failure the
    /// caller retains all entries and can release their provider references.
    pub fn commit_reserved_transfers(
        &mut self,
        reservation_id: u64,
        slots: &[u64],
        entries: Vec<TransferredHandleEntry>,
    ) -> Result<(), Vec<TransferredHandleEntry>> {
        if slots.len() != entries.len()
            || slots.iter().any(|&fd| {
                self.reserved.get(&fd) != Some(&reservation_id) || self.get_entry(fd).is_some()
            })
        {
            return Err(entries);
        }

        let max_index = slots.iter().filter_map(|&fd| dynamic_index(fd)).max();
        if let Some(max_index) = max_index
            && self.ensure_entry_capacity(max_index).is_none()
        {
            return Err(entries);
        }
        for (fd, transferred) in slots.iter().copied().zip(entries) {
            self.reserved.remove(&fd);
            if let Some(index) = standard_index(fd) {
                self.standard[index] = Some(transferred.into_entry());
            } else {
                let index = dynamic_index(fd).expect("validated descriptor reservation");
                self.entries[index] = Some(transferred.into_entry());
            }
        }
        Ok(())
    }

    pub fn display_surface_count(&self) -> usize {
        self.standard
            .iter()
            .chain(self.entries.iter())
            .flatten()
            .filter(|entry| matches!(entry.handle(), KernelHandle::DisplaySurface(_)))
            .count()
    }

    pub fn gpu_atlas_slot_in_use(&self, slot: u32) -> bool {
        self.standard
            .iter()
            .chain(self.entries.iter())
            .flatten()
            .any(|entry| {
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
        if self.reserved.contains_key(&fd) {
            return None;
        }
        if let Some(index) = standard_index(fd) {
            let replaced = core::mem::replace(&mut self.standard[index], entry);
            release_console_entry_reference(replaced.as_ref());
            return Some(());
        }
        let index = dynamic_index(fd)?;
        self.ensure_entry_capacity(index)?;
        let replaced = core::mem::replace(&mut self.entries[index], entry);
        release_console_entry_reference(replaced.as_ref());
        Some(())
    }

    pub fn close(&mut self, fd: u64) -> Option<KernelHandle> {
        let entry = if let Some(index) = standard_index(fd) {
            self.standard.get_mut(index)?.take()?
        } else {
            let index = dynamic_index(fd)?;
            self.entries.get_mut(index)?.take()?
        };
        release_console_entry_reference(Some(&entry));
        Some(entry.into_handle())
    }

    pub fn close_cloexec(&mut self) -> Vec<KernelHandle> {
        // An exec boundary invalidates receive transactions prepared by the
        // old image. Reservation ids prevent stale commit into reused slots.
        self.reserved.clear();
        let mut closed = Vec::new();
        for entry in self.standard.iter_mut().chain(self.entries.iter_mut()) {
            let Some(current) = entry.as_ref() else {
                continue;
            };
            if current.fd_flags() & FD_CLOEXEC == 0 {
                continue;
            }

            release_console_entry_reference(entry.as_ref());
            if let Some(entry) = entry.take() {
                closed.push(entry.into_handle());
            }
        }
        closed
    }

    pub fn close_all(&mut self) -> Vec<KernelHandle> {
        self.reserved.clear();
        let mut closed = Vec::new();
        for entry in self.standard.iter_mut().chain(self.entries.iter_mut()) {
            if let Some(entry) = entry.take() {
                release_console_entry_reference(Some(&entry));
                closed.push(entry.into_handle());
            }
        }
        closed
    }

    /// Stable descriptor snapshot for service-side open-description lifecycle
    /// accounting. Callers must release the process-table lock before doing
    /// IPC; the cloned handles are authority-free identifiers, not new fds.
    pub fn entries_snapshot(&self, cloexec_only: bool) -> Vec<(u64, HandleEntry)> {
        let mut snapshot = Vec::new();
        for (fd, entry) in self.standard.iter().enumerate().chain(
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (FIRST_DYNAMIC_FD as usize + index, entry)),
        ) {
            let Some(entry) = entry.as_ref() else {
                continue;
            };
            if cloexec_only && entry.fd_flags() & FD_CLOEXEC == 0 {
                continue;
            }
            snapshot.push((fd as u64, entry.clone()));
        }
        snapshot
    }

    pub fn duplicate_min(&mut self, fd: u64, min_fd: u64, close_on_exec: bool) -> Option<u64> {
        if min_fd > MAX_DYNAMIC_FD {
            return None;
        }
        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        let console = console_for_entry(&entry);
        if let Some(console) = console.as_ref() {
            console.acquire_descriptor_reference();
        }
        let installed = self.install_entry_min(entry, min_fd);
        if installed.is_none()
            && let Some(console) = console
        {
            let _ = console.release_descriptor_reference();
        }
        installed
    }

    pub fn duplicate_exact(&mut self, fd: u64, new_fd: u64, close_on_exec: bool) -> Option<u64> {
        self.duplicate_exact_with_replaced(fd, new_fd, close_on_exec)
            .map(|(fd, _)| fd)
    }

    pub fn duplicate_exact_with_replaced(
        &mut self,
        fd: u64,
        new_fd: u64,
        close_on_exec: bool,
    ) -> Option<(u64, Option<KernelHandle>)> {
        if new_fd > MAX_DYNAMIC_FD {
            return None;
        }

        if fd == new_fd {
            let entry = self.get_entry_mut(fd)?;
            entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
            return Some((new_fd, None));
        }
        if self.reserved.contains_key(&new_fd) {
            return None;
        }
        let mut entry = self.get_entry(fd)?.clone();
        entry.set_fd_flags(if close_on_exec { FD_CLOEXEC } else { 0 });
        let source_console = console_for_entry(&entry);
        if let Some(console) = source_console.as_ref() {
            console.acquire_descriptor_reference();
        }
        if let Some(index) = standard_index(new_fd) {
            let replaced = self.standard[index].replace(entry);
            release_console_entry_reference(replaced.as_ref());
            return Some((new_fd, replaced.map(HandleEntry::into_handle)));
        }
        let index = dynamic_index(new_fd)?;
        if self.ensure_entry_capacity(index).is_none() {
            if let Some(console) = source_console {
                let _ = console.release_descriptor_reference();
            }
            return None;
        }
        let replaced = self.entries[index].replace(entry);
        release_console_entry_reference(replaced.as_ref());
        Some((new_fd, replaced.map(HandleEntry::into_handle)))
    }

    pub fn clear_surface_mappings_in_range(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len) else {
            return;
        };

        for entry in self.standard.iter_mut().chain(self.entries.iter_mut()) {
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

        for entry in self.standard.iter().chain(self.entries.iter()) {
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

impl Drop for HandleTable {
    fn drop(&mut self) {
        for entry in self.standard.iter().chain(self.entries.iter()).flatten() {
            release_console_entry_reference(Some(entry));
        }
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

fn standard_index(fd: u64) -> Option<usize> {
    (fd < FIRST_DYNAMIC_FD as u64)
        .then(|| usize::try_from(fd).ok())
        .flatten()
}

fn max_dynamic_entries() -> usize {
    usize::try_from(MAX_DYNAMIC_FD - FIRST_DYNAMIC_FD as u64 + 1)
        .expect("dynamic descriptor ceiling must fit usize")
}

fn console_for_entry(entry: &HandleEntry) -> Option<ConsoleHandle> {
    match entry.handle() {
        KernelHandle::Console(console) => Some(console.clone()),
        _ => None,
    }
}

fn release_console_entry_reference(entry: Option<&HandleEntry>) {
    if let Some(entry) = entry
        && let KernelHandle::Console(console) = entry.handle()
    {
        let _ = console.release_descriptor_reference();
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{
        ConsoleStreamKind, FD_CLOEXEC, HandleEntry, HandleTable, KernelHandle, MAX_DYNAMIC_FD,
        VfsDirectoryHandle, max_dynamic_entries,
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
    fn standard_descriptors_are_real_unique_open_descriptions() {
        let table = HandleTable::new();
        let stdin = table.get(0).expect("stdin");
        let stdout = table.get(1).expect("stdout");
        let stderr = table.get(2).expect("stderr");
        assert_eq!(stdin.console_stream(), Some(ConsoleStreamKind::Input));
        assert_eq!(stdout.console_stream(), Some(ConsoleStreamKind::Output));
        assert_eq!(stderr.console_stream(), Some(ConsoleStreamKind::Error));
        assert_ne!(
            table.get_entry(0).expect("stdin entry").token(),
            table.get_entry(1).expect("stdout entry").token()
        );
        assert_ne!(
            table.get_entry(1).expect("stdout entry").token(),
            table.get_entry(2).expect("stderr entry").token()
        );
    }

    #[test]
    fn close_and_dup_reuse_standard_slots_with_one_open_description() {
        let mut table = HandleTable::new();
        let stdin_token = table.get_entry(0).expect("stdin").token();
        let stdin_description_token = match table.get(0).expect("stdin") {
            KernelHandle::Console(console) => console.token_id(),
            _ => panic!("stdin must be a console"),
        };
        let closed = table.close(1).expect("close stdout");
        assert_eq!(closed.console_stream(), Some(ConsoleStreamKind::Output));

        assert_eq!(table.duplicate_min(0, 0, false), Some(1));
        assert_eq!(
            table.get_entry(1).expect("duplicated stdin").token(),
            stdin_token
        );

        let mut child = table.clone();
        assert_eq!(
            child.get_entry(0).expect("child stdin").token(),
            stdin_token
        );
        assert_eq!(
            child.get_entry(1).expect("child duplicated stdin").token(),
            stdin_token
        );

        let _ = table.close(0).expect("parent stdin");
        let _ = table.close(1).expect("parent duplicate");
        assert!(super::ConsoleHandle::token_is_live(stdin_description_token));
        let _ = child.close(0).expect("child stdin");
        assert!(super::ConsoleHandle::token_is_live(stdin_description_token));
        let final_ref = match child.close(1).expect("child duplicate") {
            KernelHandle::Console(console) => console,
            _ => panic!("child duplicate must be a console"),
        };
        assert!(final_ref.is_last_reference());
        assert!(!super::ConsoleHandle::token_is_live(
            stdin_description_token
        ));
    }

    #[test]
    fn console_last_close_ignores_transient_handle_snapshot() {
        let mut table = HandleTable::new();
        let snapshot = match table.get(0).expect("stdin").clone() {
            KernelHandle::Console(console) => console,
            _ => panic!("stdin must be a console"),
        };
        let token = snapshot.token_id();

        let closed = match table.close(0).expect("close stdin") {
            KernelHandle::Console(console) => console,
            _ => panic!("closed stdin must be a console"),
        };
        assert!(closed.is_last_reference());
        assert!(!super::ConsoleHandle::token_is_live(token));
        assert_eq!(super::ConsoleHandle::stream_for_token(token), None);

        drop(snapshot);
        drop(closed);
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

        let closed = table.close_cloexec();

        assert!(table.get(keep_fd.expect("keep descriptor")).is_some());
        assert!(table.get(drop_fd.expect("cloexec descriptor")).is_none());
        assert_eq!(closed.len(), 1);
        assert!(matches!(
            &closed[0],
            KernelHandle::VfsDirectory(directory) if directory.path() == "/drop"
        ));
    }

    #[test]
    fn lifecycle_snapshot_is_descriptor_exact_and_filters_cloexec() {
        let mut table = HandleTable::new();
        let keep_fd = table
            .install_entry(HandleEntry::new(
                KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/keep".into(), vec![])),
                0,
                0,
            ))
            .unwrap();
        let cloexec_fd = table
            .install_entry(HandleEntry::new(
                KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/exec".into(), vec![])),
                FD_CLOEXEC,
                0,
            ))
            .unwrap();

        let all = table.entries_snapshot(false);
        assert_eq!(
            all.iter()
                .map(|(fd, _)| *fd)
                .collect::<alloc::vec::Vec<_>>(),
            vec![0, 1, 2, keep_fd, cloexec_fd]
        );
        let cloexec = table.entries_snapshot(true);
        assert_eq!(cloexec.len(), 1);
        assert_eq!(cloexec[0].0, cloexec_fd);
    }

    #[test]
    fn receive_reservations_are_invisible_and_publish_atomically() {
        let mut table = HandleTable::new();
        let _ = table.close(0).expect("free standard descriptor");
        let (reservation_id, slots) = table.reserve_slots(2).expect("reserve receive slots");
        assert_eq!(slots, vec![0, 3]);
        assert!(table.is_reserved(0));
        assert!(table.is_reserved(3));
        assert!(table.get_entry(0).is_none());
        assert!(table.get_entry(3).is_none());
        assert!(table.duplicate_exact(1, 3, false).is_none());
        assert!(
            table
                .replace_entry(
                    3,
                    Some(HandleEntry::new(
                        KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
                            "/replacement".into(),
                            vec![],
                        )),
                        0,
                        0,
                    )),
                )
                .is_none()
        );

        let mut child = table.clone();
        assert_eq!(
            child.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
                "/child".into(),
                vec![],
            ))),
            Some(0),
            "fork must not inherit a parent's in-flight receive transaction"
        );

        let unrelated = table
            .install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
                "/unrelated".into(),
                vec![],
            )))
            .expect("ordinary install must skip reservations");
        assert_eq!(unrelated, 4);

        let entries = ["/first", "/second"]
            .into_iter()
            .map(|path| {
                super::TransferredHandleEntry::from_entry(HandleEntry::new(
                    KernelHandle::VfsDirectory(VfsDirectoryHandle::new(path.into(), vec![])),
                    0,
                    0,
                ))
                .expect("transferable directory")
            })
            .collect();
        table
            .commit_reserved_transfers(reservation_id, &slots, entries)
            .expect("commit reservations");
        assert!(table.get_entry(0).is_some());
        assert!(table.get_entry(3).is_some());
        assert!(!table.is_reserved(0));
        assert!(!table.is_reserved(3));
    }

    #[test]
    fn cancelled_receive_reservation_is_reusable() {
        let mut table = HandleTable::new();
        let (reservation_id, slots) = table.reserve_slots(1).expect("reserve receive slot");
        assert_eq!(slots, vec![3]);
        table.cancel_reservations(reservation_id, &slots);
        assert_eq!(
            table.install(KernelHandle::VfsDirectory(VfsDirectoryHandle::new(
                "/reuse".into(),
                vec![],
            ))),
            Some(3)
        );
    }

    #[test]
    fn stale_reservation_cannot_cancel_or_commit_after_exec_boundary() {
        let mut table = HandleTable::new();
        let _ = table.close(0).expect("free standard descriptor");
        let (stale_id, stale_slots) = table.reserve_slots(1).expect("old reservation");
        assert_eq!(stale_slots, vec![0]);

        let _closed = table.close_all();
        let (live_id, live_slots) = table.reserve_slots(1).expect("new reservation");
        assert_eq!(live_slots, vec![0]);
        assert_ne!(stale_id, live_id);
        table.cancel_reservations(stale_id, &stale_slots);

        let entries = vec![
            super::TransferredHandleEntry::from_entry(HandleEntry::new(
                KernelHandle::VfsDirectory(VfsDirectoryHandle::new("/received".into(), vec![])),
                0,
                0,
            ))
            .expect("transferable directory"),
        ];
        let entries = table
            .commit_reserved_transfers(stale_id, &stale_slots, entries)
            .expect_err("stale transaction must not commit");
        table
            .commit_reserved_transfers(live_id, &live_slots, entries)
            .expect("live transaction commits");
        assert!(table.get_entry(0).is_some());
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

        let (duplicated_fd, retired) = table
            .duplicate_exact_with_replaced(
                source_fd.expect("source descriptor"),
                target_fd.expect("target descriptor"),
                true,
            )
            .expect("dup2-style replace");
        assert_eq!(Some(duplicated_fd), target_fd);
        match retired.expect("exact target must be returned") {
            KernelHandle::VfsDirectory(dir) => assert_eq!(dir.path(), "/target"),
            other => panic!("expected retired VfsDirectory, got {other:?}"),
        }
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
                kernel_object::api::device::DeviceId::Display,
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
