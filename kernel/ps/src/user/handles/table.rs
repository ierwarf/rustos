//! Process descriptor table and open-description capability lifetime.
//!
//! - **Owner:** `kernel-ps` owns descriptor slots; provider services own the
//!   referenced open descriptions.
//! - **Boundary:** User fd numbers are untrusted indices and never provider
//!   identities.
//! - **Lifecycle:** Install, dup/fork retain, close-on-exec filter, transfer,
//!   final close, and provider settlement preserve one exact reference count.
//! - **Concurrency:** Table mutation is process-serialized; provider callbacks
//!   and deferred settlement run after local mutation state is released.
//! - **Failure:** Capacity, stale token, transfer, and provider-restart errors
//!   leave the source descriptor and reference ledger consistent.
//! - **Forbidden:** No fabricated standard descriptors, close-as-success after
//!   lost settlement, or slot reuse before exact reference withdrawal.
//! - **Evidence:** `ipc-handle-transfer` and `vfs-open-description`.
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
        // AF_INET has the same service-owned open-description lifetime as a
        // local socket.  Its first reference is created directly in the
        // transfer registry, so this table-level admission is the single
        // capability gate for both that publication and later fd transfer.
        self.handle.supports_descriptor_transfer(self.rights)
            || matches!(&self.handle, KernelHandle::InetSocket(_)) && self.rights.allows_transfer()
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
        if !acquire_entry_descriptor_reference(&entry) {
            return None;
        }
        Some(Self { entry: Some(entry) })
    }

    /// Wraps the first provider-owned descriptor reference for deferred IPC
    /// publication without acquiring a second provider reference.
    ///
    /// The caller must have just created the provider object with exactly one
    /// live reference and must either bind this entry to a reply or reclaim it
    /// before asking that provider to discard the object.  Ordinary fd export
    /// uses [`Self::from_entry`] instead because an already-installed source
    /// descriptor needs an additional reference while both entries coexist.
    pub fn from_initial_entry(entry: HandleEntry) -> Option<Self> {
        entry
            .supports_transfer()
            .then_some(Self { entry: Some(entry) })
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
        release_entry_descriptor_reference(self.entry.as_ref());
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
            assert!(
                acquire_entry_descriptor_reference(entry),
                "fork cloned a stale descriptor-backed kernel object"
            );
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
        self.reserve_slots_faultable(
            count,
            nucleus_core::util::fault_injection::should_fail("handle.reserve"),
        )
    }

    fn reserve_slots_faultable(
        &mut self,
        count: usize,
        injected_failure: bool,
    ) -> Option<(u64, Vec<u64>)> {
        if injected_failure {
            return None;
        }
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
        self.commit_reserved_transfers_faultable(
            reservation_id,
            slots,
            entries,
            nucleus_core::util::fault_injection::should_fail("handle.commit"),
        )
    }

    fn commit_reserved_transfers_faultable(
        &mut self,
        reservation_id: u64,
        slots: &[u64],
        entries: Vec<TransferredHandleEntry>,
        injected_failure: bool,
    ) -> Result<(), Vec<TransferredHandleEntry>> {
        if slots.len() != entries.len()
            || slots.iter().any(|&fd| {
                self.reserved.get(&fd) != Some(&reservation_id) || self.get_entry(fd).is_some()
            })
            || injected_failure
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
            release_entry_descriptor_reference(replaced.as_ref());
            return Some(());
        }
        let index = dynamic_index(fd)?;
        self.ensure_entry_capacity(index)?;
        let replaced = core::mem::replace(&mut self.entries[index], entry);
        release_entry_descriptor_reference(replaced.as_ref());
        Some(())
    }

    pub fn close(&mut self, fd: u64) -> Option<KernelHandle> {
        let entry = if let Some(index) = standard_index(fd) {
            self.standard.get_mut(index)?.take()?
        } else {
            let index = dynamic_index(fd)?;
            self.entries.get_mut(index)?.take()?
        };
        release_entry_descriptor_reference(Some(&entry));
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

            release_entry_descriptor_reference(entry.as_ref());
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
                release_entry_descriptor_reference(Some(&entry));
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
        if !acquire_entry_descriptor_reference(&entry) {
            return None;
        }
        let installed = self.install_entry_min(entry, min_fd);
        if installed.is_none() {
            release_entry_descriptor_reference(self.get_entry(fd));
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
        if !acquire_entry_descriptor_reference(&entry) {
            return None;
        }
        if let Some(index) = standard_index(new_fd) {
            let replaced = self.standard[index].replace(entry);
            release_entry_descriptor_reference(replaced.as_ref());
            return Some((new_fd, replaced.map(HandleEntry::into_handle)));
        }
        let index = dynamic_index(new_fd)?;
        if self.ensure_entry_capacity(index).is_none() {
            release_entry_descriptor_reference(Some(&entry));
            return None;
        }
        let replaced = self.entries[index].replace(entry);
        release_entry_descriptor_reference(replaced.as_ref());
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
            release_entry_descriptor_reference(Some(entry));
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

fn acquire_entry_descriptor_reference(entry: &HandleEntry) -> bool {
    match entry.handle() {
        KernelHandle::Console(console) => {
            console.acquire_descriptor_reference();
            true
        }
        KernelHandle::DisplaySurface(surface) => surface
            .shared_region()
            .is_none_or(crate::ipc::retain_shared_region_descriptor),
        _ => true,
    }
}

fn release_entry_descriptor_reference(entry: Option<&HandleEntry>) {
    let Some(entry) = entry else {
        return;
    };
    match entry.handle() {
        KernelHandle::Console(console) => {
            let _ = console.release_descriptor_reference();
        }
        KernelHandle::DisplaySurface(surface) => {
            if let Some(region) = surface.shared_region() {
                crate::ipc::release_shared_region_descriptor(region);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "table/tests.rs"]
mod tests;
