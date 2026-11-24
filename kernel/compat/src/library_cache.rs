//! Owner: compat records shared immutable-file objects that `pagerd` supplies.
//! Boundary: pagerd names an identity and supplies bytes; vfsd attests which
//! open description is that file. Neither a path nor a basename is authority.
//! Lifecycle: create -> supply every page exactly once -> seal -> serve ->
//! retire. A retired or never-sealed object serves nothing; a guest binding to
//! it fails closed and falls back to the eager copy path.
//! Concurrency: one registry lock, never held across VFS IPC, a process-state
//! lock or any user copy. Page pins are atomic frame-source references.
//! Failure: a pagerd lease change withdraws every frame source, so pinning a
//! page fails and the whole mapping falls back; there is no partial serve.
//! Forbidden: cache policy (what to warm, what to evict), file I/O, paths.
//!
//! Two object kinds share the slot table and the frame-source namespace:
//! *warm* objects are built whole by pagerd (create, supply every page, seal);
//! *demand* objects are created by the MM broker for one exact vfsd-attested
//! immutable file and hold only the pages some mapping has needed so far. A page
//! of a demand object is first *claimed* (so concurrent mappers never read the
//! same range twice), then supplied exactly once. A demand object serves a
//! mapping only when every page of the range is resident; the broker fills the
//! gaps before it pins. Demand residency is bounded and evicted least recently
//! used first; eviction only withdraws the cache's own reference, so mapped
//! leaves keep their frames until their own release.
//! Evidence: the registry model tests below and the mm-broker alias tests.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use kernel_mm::api::frame_source::{FrameSourceError, FrameSourceKey, FrameSourceOwner};
use kernel_mm::api::kmem_charge::{Class, Meter};
use rustos_user_abi::syscall::{
    LibraryIdentityWire, VfsFileIdentityWire, LIBRARY_OBJECT_BACKING_NAMESPACE,
    LIBRARY_DEMAND_MAX_OBJECTS, LIBRARY_DEMAND_MAX_OBJECT_PAGES,
    LIBRARY_DEMAND_MAX_RESIDENT_PAGES, LIBRARY_OBJECT_MAX_OBJECTS, LIBRARY_OBJECT_MAX_PAGES,
};

const PAGE: u64 = 4096;
const SLOT_BITS: u32 = 16;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;
/// Pages that may be reserved by live objects at once. Leaves room for the
/// frame-source table to also serve other suppliers.
pub(crate) const MAX_RESERVED_PAGES: u64 = 12_288;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LibraryError {
    Invalid,
    Exists,
    NoSlots,
    Budget,
    Stale,
    Authority,
    Incomplete,
    Sealed,
    Source(FrameSourceError),
}

/// The frame-source operations the registry needs. The kernel implementation
/// forwards to `kernel_mm`; tests substitute a model.
pub(crate) trait PageSources {
    type Pin;
    fn supply(
        &self,
        owner: FrameSourceOwner,
        key: FrameSourceKey,
        group: usize,
        bytes: &[u8],
    ) -> Result<u64, FrameSourceError>;
    fn pin(
        &self,
        token: u64,
        owner: FrameSourceOwner,
        key: FrameSourceKey,
    ) -> Result<Self::Pin, FrameSourceError>;
    fn retire(&self, token: u64, owner: FrameSourceOwner);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Building,
    Sealed,
    /// Demand object: servable page by page once every page of a range is
    /// resident; never sealed and never filled by pagerd.
    Demand,
}

/// A demand page reserved by one filler and not yet supplied. Never a valid
/// frame-source token (tokens are `generation << index_bits | index + 1`).
const CLAIMED: u64 = u64::MAX;
const CHUNK_PAGES: u64 = 512;
const CHUNK_BYTES: u64 = CHUNK_PAGES * core::mem::size_of::<u64>() as u64;

/// Frame-source token per page. Warm objects are small and dense; a demand
/// object may describe a file of hundreds of MiB of which only the mapped
/// ranges are resident, so its token words are allocated per 512-page chunk.
enum Tokens {
    Dense(Vec<u64>),
    Sparse { pages: u64, chunks: Vec<Option<Box<[u64]>>> },
}

impl Tokens {
    fn len(&self) -> u64 {
        match self {
            Tokens::Dense(tokens) => tokens.len() as u64,
            Tokens::Sparse { pages, .. } => *pages,
        }
    }

    fn get(&self, page: u64) -> u64 {
        match self {
            Tokens::Dense(tokens) => tokens[page as usize],
            Tokens::Sparse { chunks, .. } => chunks[(page / CHUNK_PAGES) as usize]
                .as_ref()
                .map_or(0, |chunk| chunk[(page % CHUNK_PAGES) as usize]),
        }
    }

    /// Live frame-source tokens (neither empty nor merely claimed).
    fn live(&self) -> Vec<u64> {
        let mut out = Vec::new();
        match self {
            Tokens::Dense(tokens) => {
                out.extend(tokens.iter().copied().filter(|token| *token != 0 && *token != CLAIMED))
            }
            Tokens::Sparse { chunks, .. } => {
                for chunk in chunks.iter().flatten() {
                    out.extend(chunk.iter().copied().filter(|token| *token != 0 && *token != CLAIMED));
                }
            }
        }
        out
    }
}

struct Object {
    generation: u64,
    owner: FrameSourceOwner,
    identity: LibraryIdentityWire,
    /// Frame-source token per page; zero until supplied.
    tokens: Tokens,
    supplied: u64,
    /// Demand objects: pages that are supplied or claimed (resident or about
    /// to be). Counted against the registry's demand residency bound.
    resident: u64,
    /// Registry tick of the last lookup, for least-recently-used eviction.
    last_used: AtomicU64,
    state: State,
    /// Registry heap (`tokens`) billed to the creating service's Job. The
    /// meter refunds on drop, and an object is dropped exactly once when it
    /// leaves its slot (retire or a replaced pager lease). Meter drop is
    /// lock-free, so it is legal under the registry lock.
    meter: Meter,
}

impl Object {
    /// Stores `token` for `page`, allocating (and billing) the page's chunk on
    /// first use. Dense objects never allocate here.
    fn set_token(&mut self, page: u64, token: u64) -> Result<(), LibraryError> {
        match &mut self.tokens {
            Tokens::Dense(tokens) => tokens[page as usize] = token,
            Tokens::Sparse { chunks, .. } => {
                let index = (page / CHUNK_PAGES) as usize;
                if chunks[index].is_none() {
                    if !self.meter.set_total(self.meter.charged() + CHUNK_BYTES) {
                        return Err(LibraryError::Budget);
                    }
                    let mut chunk = Vec::new();
                    if chunk.try_reserve_exact(CHUNK_PAGES as usize).is_err() {
                        self.meter.set_total(self.meter.charged() - CHUNK_BYTES);
                        return Err(LibraryError::Budget);
                    }
                    chunk.resize(CHUNK_PAGES as usize, 0);
                    chunks[index] = Some(chunk.into_boxed_slice());
                }
                chunks[index].as_mut().expect("allocated above")[(page % CHUNK_PAGES) as usize] = token;
            }
        }
        Ok(())
    }
}

pub(crate) struct Registry {
    slots: [Option<Object>; LIBRARY_OBJECT_MAX_OBJECTS],
    next_generation: [u64; LIBRARY_OBJECT_MAX_OBJECTS],
    reserved_pages: u64,
    /// Pages resident or claimed across demand objects, and the bounds on them.
    demand_resident: u64,
    demand_resident_limit: u64,
    demand_object_limit: usize,
    tick: AtomicU64,
    /// Advances whenever an object is sealed or retired. A guest binding that
    /// recorded "no object for this file" is only valid for the epoch it saw.
    epoch: u64,
}

/// What a sealed object contributes to a guest VMA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectView {
    pub handle: u64,
    pub len: u64,
    pub backing_service: u64,
    pub slot: u64,
    pub generation: u64,
    pub backing_generation: u64,
    pub owner_generation: u64,
}

pub(crate) const fn handle_slot(handle: u64) -> Option<usize> {
    let slot = handle & SLOT_MASK;
    if slot == 0 || slot > LIBRARY_OBJECT_MAX_OBJECTS as u64 {
        None
    } else {
        Some(slot as usize - 1)
    }
}
const fn handle_generation(handle: u64) -> u64 {
    handle >> SLOT_BITS
}
const fn make_handle(slot: usize, generation: u64) -> u64 {
    (generation << SLOT_BITS) | (slot as u64 + 1)
}

fn page_key(slot: usize, generation: u64, page: u64) -> FrameSourceKey {
    FrameSourceKey {
        namespace: LIBRARY_OBJECT_BACKING_NAMESPACE,
        object: slot as u64 + 1,
        object_generation: generation,
        content_generation: 1,
        page_offset: page * PAGE,
    }
}

impl Registry {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { None }; LIBRARY_OBJECT_MAX_OBJECTS],
            next_generation: [1; LIBRARY_OBJECT_MAX_OBJECTS],
            reserved_pages: 0,
            demand_resident: 0,
            demand_resident_limit: LIBRARY_DEMAND_MAX_RESIDENT_PAGES,
            demand_object_limit: LIBRARY_DEMAND_MAX_OBJECTS,
            tick: AtomicU64::new(0),
            epoch: 1,
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether any object could possibly serve a guest mapping. Lets the broker
    /// skip vfsd attestation entirely on a system whose pager never warmed.
    pub(crate) fn has_sealed_objects(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|object| matches!(object.state, State::Sealed | State::Demand))
    }

    pub(crate) fn is_demand(&self, handle: u64) -> bool {
        self.object(handle)
            .is_ok_and(|(_, object)| object.state == State::Demand)
    }

    fn object(&self, handle: u64) -> Result<(usize, &Object), LibraryError> {
        let slot = handle_slot(handle).ok_or(LibraryError::Invalid)?;
        match &self.slots[slot] {
            Some(object) if object.generation == handle_generation(handle) => Ok((slot, object)),
            _ => Err(LibraryError::Stale),
        }
    }

    /// Reserves an object for `identity`. Objects of a previous pager lease are
    /// dropped first: their sources were withdrawn with that lease, so nothing
    /// of theirs can ever be pinned again.
    pub(crate) fn create(
        &mut self,
        owner: FrameSourceOwner,
        group: usize,
        identity: LibraryIdentityWire,
    ) -> Result<u64, LibraryError> {
        if !owner.is_valid() || !identity.is_canonical() {
            return Err(LibraryError::Invalid);
        }
        self.drop_foreign_owner(owner);
        if self
            .slots
            .iter()
            .flatten()
            .any(|object| object.identity.file == identity.file)
        {
            return Err(LibraryError::Exists);
        }
        let pages = identity.page_count();
        if pages == 0 || pages > LIBRARY_OBJECT_MAX_PAGES {
            return Err(LibraryError::Invalid);
        }
        if self.reserved_pages + pages > MAX_RESERVED_PAGES {
            return Err(LibraryError::Budget);
        }
        let Some(slot) = self.slots.iter().position(Option::is_none) else {
            return Err(LibraryError::NoSlots);
        };
        let generation = self.next_generation[slot];
        let Some(next) = generation.checked_add(1) else {
            return Err(LibraryError::NoSlots);
        };
        // Bill the owning service's Job before allocating; a refusal leaves
        // nothing charged and nothing published. If the allocation then fails
        // the dropped meter refunds.
        let mut meter = Meter::new(Class::LibraryObject);
        if !meter.bind(group, pages * core::mem::size_of::<u64>() as u64) {
            return Err(LibraryError::Budget);
        }
        let mut tokens = Vec::new();
        tokens
            .try_reserve_exact(pages as usize)
            .map_err(|_| LibraryError::Budget)?;
        tokens.resize(pages as usize, 0);
        self.next_generation[slot] = next;
        self.reserved_pages += pages;
        self.slots[slot] = Some(Object {
            generation,
            owner,
            identity,
            tokens: Tokens::Dense(tokens),
            supplied: 0,
            resident: 0,
            last_used: AtomicU64::new(0),
            state: State::Building,
            meter,
        });
        Ok(make_handle(slot, generation))
    }

    pub(crate) fn retire_owner(&mut self, owner: FrameSourceOwner) {
        self.drop_matching_owners(|object| object.owner == owner);
    }

    fn drop_foreign_owner(&mut self, owner: FrameSourceOwner) {
        self.drop_matching_owners(|object| object.owner != owner);
    }

    fn drop_matching_owners(&mut self, mut predicate: impl FnMut(&Object) -> bool) {
        let mut dropped = false;
        for slot in 0..LIBRARY_OBJECT_MAX_OBJECTS {
            if self.slots[slot].as_ref().is_some_and(&mut predicate) {
                let object = self.slots[slot].take().expect("checked above");
                self.release_accounting(&object);
                dropped = true;
            }
        }
        if dropped {
            self.epoch += 1;
        }
    }

    /// Supplies page `page` of an unsealed object. The payload must be exactly
    /// the bytes of that page that exist in the file; the kernel zero-fills the
    /// tail of the last page, so a pager cannot place a non-zero byte beyond EOF.
    pub(crate) fn supply_page<S: PageSources>(
        &mut self,
        sources: &S,
        handle: u64,
        owner: FrameSourceOwner,
        page: u64,
        group: usize,
        bytes: &[u8],
    ) -> Result<u64, LibraryError> {
        let (slot, object) = self.object(handle)?;
        if object.owner != owner {
            return Err(LibraryError::Authority);
        }
        if object.state != State::Building {
            return Err(LibraryError::Sealed);
        }
        let pages = object.tokens.len();
        if page >= pages || object.tokens.get(page) != 0 {
            return Err(LibraryError::Invalid);
        }
        let expected = (object.identity.file.len - page * PAGE).min(PAGE);
        if bytes.len() as u64 != expected {
            return Err(LibraryError::Invalid);
        }
        let key = page_key(slot, object.generation, page);
        let token = sources
            .supply(owner, key, group, bytes)
            .map_err(LibraryError::Source)?;
        let object = self.slots[slot].as_mut().expect("validated above");
        object.set_token(page, token)?;
        object.supplied += 1;
        Ok(token)
    }

    pub(crate) fn seal(&mut self, handle: u64, owner: FrameSourceOwner) -> Result<(), LibraryError> {
        let (slot, object) = self.object(handle)?;
        if object.owner != owner {
            return Err(LibraryError::Authority);
        }
        if object.state != State::Building {
            return Err(LibraryError::Sealed);
        }
        if object.supplied != object.tokens.len() {
            return Err(LibraryError::Incomplete);
        }
        self.slots[slot].as_mut().expect("validated above").state = State::Sealed;
        self.epoch += 1;
        Ok(())
    }

    pub(crate) fn retire<S: PageSources>(
        &mut self,
        sources: &S,
        handle: u64,
        owner: FrameSourceOwner,
    ) -> Result<(), LibraryError> {
        let (slot, object) = self.object(handle)?;
        if object.owner != owner {
            return Err(LibraryError::Authority);
        }
        let object = self.slots[slot].take().expect("validated above");
        for token in object.tokens.live() {
            sources.retire(token, owner);
        }
        self.release_accounting(&object);
        self.epoch += 1;
        Ok(())
    }

    /// Returns what a dropped object had reserved (warm pages) or held (demand
    /// residency). Frame sources are retired separately by the caller.
    fn release_accounting(&mut self, object: &Object) {
        if object.state == State::Demand {
            self.demand_resident -= object.resident;
        } else {
            self.reserved_pages -= object.tokens.len();
        }
    }

    /// The sealed object that holds exactly this attested file, if any. Every
    /// identity field participates; a different generation, mount, storage
    /// epoch or length is a different file.
    pub(crate) fn find(&self, file: &VfsFileIdentityWire) -> Option<ObjectView> {
        self.slots.iter().enumerate().find_map(|(slot, object)| {
            let object = object.as_ref()?;
            (matches!(object.state, State::Sealed | State::Demand)
                && object.identity.file == *file)
                .then(|| {
                    self.touch(object);
                    view(slot, object)
                })
        })
    }

    /// Revalidates a previously bound handle.
    pub(crate) fn view(&self, handle: u64) -> Result<ObjectView, LibraryError> {
        let (slot, object) = self.object(handle)?;
        if object.state == State::Building {
            return Err(LibraryError::Stale);
        }
        self.touch(object);
        Ok(view(slot, object))
    }

    /// Pins `count` consecutive pages starting at `first_page`. All or nothing.
    pub(crate) fn pin_range<S: PageSources>(
        &self,
        sources: &S,
        handle: u64,
        first_page: u64,
        count: u64,
    ) -> Result<Vec<S::Pin>, LibraryError> {
        let (slot, object) = self.object(handle)?;
        if object.state == State::Building {
            return Err(LibraryError::Stale);
        }
        let end = first_page.checked_add(count).ok_or(LibraryError::Invalid)?;
        if count == 0 || end > object.tokens.len() {
            return Err(LibraryError::Invalid);
        }
        self.touch(object);
        let mut pins = Vec::new();
        pins.try_reserve_exact(count as usize)
            .map_err(|_| LibraryError::Budget)?;
        for page in first_page..end {
            let token = object.tokens.get(page);
            if token == 0 || token == CLAIMED {
                // A demand page that is not resident yet: the broker fills the
                // gap before it pins, so this is a race with eviction.
                return Err(LibraryError::Incomplete);
            }
            let pin = sources
                .pin(token, object.owner, page_key(slot, object.generation, page))
                .map_err(LibraryError::Source)?;
            pins.push(pin);
        }
        Ok(pins)
    }
}

impl Registry {
    #[cfg(test)]
    fn touch_for_test(&self, handle: u64) {
        if let Ok((_, object)) = self.object(handle) {
            self.touch(object);
        }
    }

    fn touch(&self, object: &Object) {
        // ORDERING: recency is a heuristic; nothing synchronizes through it.
        let tick = self.tick.fetch_add(1, Ordering::Relaxed) + 1;
        object.last_used.store(tick, Ordering::Relaxed);
    }

    /// Whether any object could serve a mapping after a fill (demand objects
    /// exist or may be created), as opposed to only already-sealed ones.
    pub(crate) fn demand_object_count(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .filter(|object| object.state == State::Demand)
            .count()
    }

    /// `(demand objects, resident or claimed demand pages)` for tests.
    #[cfg(test)]
    pub(crate) fn demand_census(&self) -> (u64, u64) {
        (self.demand_object_count() as u64, self.demand_resident)
    }

    /// Retires the least recently used demand object other than `keep`.
    fn evict_one_demand<S: PageSources>(&mut self, sources: &S, keep: Option<u64>) -> bool {
        let victim = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot, object)| {
                let object = object.as_ref()?;
                (object.state == State::Demand
                    && keep != Some(make_handle(slot, object.generation)))
                .then(|| (object.last_used.load(Ordering::Relaxed), slot))
            })
            .min();
        let Some((_, slot)) = victim else { return false };
        let object = self.slots[slot].take().expect("victim slot is live");
        for token in object.tokens.live() {
            sources.retire(token, object.owner);
        }
        self.release_accounting(&object);
        self.epoch += 1;
        true
    }

    /// Creates the demand object of one attested immutable file. Evicts least
    /// recently used demand objects when the object bound is reached.
    pub(crate) fn create_demand<S: PageSources>(
        &mut self,
        sources: &S,
        owner: FrameSourceOwner,
        group: usize,
        file: &VfsFileIdentityWire,
    ) -> Result<u64, LibraryError> {
        if !owner.is_valid() || !file.is_canonical() || !file.is_immutable() {
            return Err(LibraryError::Invalid);
        }
        self.drop_foreign_owner(owner);
        if self.slots.iter().flatten().any(|object| object.identity.file == *file) {
            return Err(LibraryError::Exists);
        }
        let pages = file.len.div_ceil(PAGE);
        if pages == 0 || pages > LIBRARY_DEMAND_MAX_OBJECT_PAGES {
            return Err(LibraryError::Invalid);
        }
        while self.demand_object_count() >= self.demand_object_limit {
            if !self.evict_one_demand(sources, None) {
                return Err(LibraryError::NoSlots);
            }
        }
        let slot = loop {
            if let Some(slot) = self.slots.iter().position(Option::is_none) {
                break slot;
            }
            if !self.evict_one_demand(sources, None) {
                return Err(LibraryError::NoSlots);
            }
        };
        let generation = self.next_generation[slot];
        let Some(next) = generation.checked_add(1) else {
            return Err(LibraryError::NoSlots);
        };
        let chunk_count = pages.div_ceil(CHUNK_PAGES) as usize;
        let mut meter = Meter::new(Class::LibraryObject);
        if !meter.bind(group, chunk_count as u64 * core::mem::size_of::<Option<Box<[u64]>>>() as u64) {
            return Err(LibraryError::Budget);
        }
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(chunk_count)
            .map_err(|_| LibraryError::Budget)?;
        chunks.resize_with(chunk_count, || None);
        self.next_generation[slot] = next;
        self.slots[slot] = Some(Object {
            generation,
            owner,
            identity: LibraryIdentityWire { file: *file, content_hash: [0; 32] },
            tokens: Tokens::Sparse { pages, chunks },
            supplied: 0,
            resident: 0,
            last_used: AtomicU64::new(0),
            state: State::Demand,
            meter,
        });
        self.touch(self.slots[slot].as_ref().expect("just published"));
        self.epoch += 1;
        Ok(make_handle(slot, generation))
    }

    /// Reserves the next run of missing pages of `[from, end)` for the caller to
    /// read and supply. `resume` is the first page that was not yet resident
    /// when the scan ran; pages never leave residency while the object lives, so
    /// the next call may start there.
    pub(crate) fn claim_gap<S: PageSources>(
        &mut self,
        sources: &S,
        handle: u64,
        from: u64,
        end: u64,
        max_run: u64,
    ) -> Result<GapClaim, LibraryError> {
        let (_, object) = self.object(handle)?;
        if object.state != State::Demand || from > end || end > object.tokens.len() || max_run == 0 {
            return Err(LibraryError::Invalid);
        }
        self.touch(object);
        let mut resume = None;
        let mut start = None;
        let mut page = from;
        while page < end {
            let token = object.tokens.get(page);
            if token == 0 {
                resume.get_or_insert(page);
                start = Some(page);
                break;
            }
            if token == CLAIMED {
                resume.get_or_insert(page);
            }
            page += 1;
        }
        let Some(start) = start else {
            return Ok(match resume {
                Some(resume) => GapClaim::Wait { resume },
                None => GapClaim::Done,
            });
        };
        let mut run = 0;
        while start + run < end && run < max_run && object.tokens.get(start + run) == 0 {
            run += 1;
        }
        // Residency bound: make room by retiring other demand objects.
        while self.demand_resident + run > self.demand_resident_limit {
            if !self.evict_one_demand(sources, Some(handle)) {
                return Err(LibraryError::Budget);
            }
        }
        let (slot, _) = self.object(handle)?;
        let object = self.slots[slot].as_mut().expect("validated above");
        for index in 0..run {
            if let Err(error) = object.set_token(start + index, CLAIMED) {
                for undo in 0..index {
                    let _ = object.set_token(start + undo, 0);
                }
                return Err(error);
            }
        }
        object.resident += run;
        self.demand_resident += run;
        Ok(GapClaim::Run { start, pages: run, resume: resume.unwrap_or(start) })
    }

    /// Supplies one claimed page of a demand object, exactly the bytes of the
    /// file that exist in it (the kernel zero-fills a short last page).
    pub(crate) fn supply_claimed<S: PageSources>(
        &mut self,
        sources: &S,
        handle: u64,
        owner: FrameSourceOwner,
        page: u64,
        group: usize,
        bytes: &[u8],
    ) -> Result<u64, LibraryError> {
        let (slot, object) = self.object(handle)?;
        if object.owner != owner {
            return Err(LibraryError::Authority);
        }
        if object.state != State::Demand || page >= object.tokens.len() || object.tokens.get(page) != CLAIMED {
            return Err(LibraryError::Invalid);
        }
        let expected = (object.identity.file.len - page * PAGE).min(PAGE);
        if bytes.len() as u64 != expected {
            return Err(LibraryError::Invalid);
        }
        let key = page_key(slot, object.generation, page);
        let token = sources
            .supply(owner, key, group, bytes)
            .map_err(LibraryError::Source)?;
        let object = self.slots[slot].as_mut().expect("validated above");
        object.set_token(page, token)?;
        object.supplied += 1;
        Ok(token)
    }

    /// Releases the still-claimed pages of `[start, start + pages)` after a
    /// failed or abandoned fill. A retired object has nothing to release.
    pub(crate) fn unclaim(&mut self, handle: u64, start: u64, pages: u64) {
        let Ok((slot, object)) = self.object(handle) else { return };
        if object.state != State::Demand {
            return;
        }
        let mut released = 0;
        let end = (start + pages).min(object.tokens.len());
        let object = self.slots[slot].as_mut().expect("validated above");
        for page in start..end {
            if object.tokens.get(page) == CLAIMED && object.set_token(page, 0).is_ok() {
                released += 1;
            }
        }
        object.resident -= released;
        self.demand_resident -= released;
    }
}

/// What `claim_gap` found in a range of a demand object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GapClaim {
    /// Every page of the range is resident.
    Done,
    /// The remaining gaps are claimed by other fillers; try again shortly.
    Wait { resume: u64 },
    /// The caller now owns `[start, start + pages)` and must supply or unclaim it.
    Run { start: u64, pages: u64, resume: u64 },
}

fn view(slot: usize, object: &Object) -> ObjectView {
    ObjectView {
        handle: make_handle(slot, object.generation),
        len: object.identity.file.len,
        backing_service: LIBRARY_OBJECT_BACKING_NAMESPACE,
        slot: slot as u64 + 1,
        generation: object.generation,
        backing_generation: 1,
        owner_generation: object.owner.service_generation,
    }
}

/// What the MM broker's bind learned about one descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BindDecision {
    Bound(u64),
    NotEligible { registry_epoch: u64 },
}

/// Pure decision core of the bind: which registry object, if any, a vfsd
/// attestation selects. Every identity field and the immutable-mount flag must
/// agree, and the length vfsd attested must be the length the guest sees.
pub(crate) fn decide_bind(
    registry: &Registry,
    file: &VfsFileIdentityWire,
    guest_len: u64,
) -> BindDecision {
    let registry_epoch = registry.epoch();
    if !file.is_canonical() || !file.is_immutable() || file.len != guest_len {
        return BindDecision::NotEligible { registry_epoch };
    }
    match registry.find(file) {
        Some(view) if view.len == guest_len => BindDecision::Bound(view.handle),
        _ => BindDecision::NotEligible { registry_epoch },
    }
}

pub(crate) static REGISTRY: spin::Mutex<Registry> = spin::Mutex::new(Registry::new());

/// Guest mappings served by aliasing a sealed object, and the pages they hold.
/// Monotonic diagnostics only; no decision reads them.
static ALIAS_MAPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static ALIAS_PAGES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub(crate) fn note_alias_map(pages: u64) {
    // ORDERING: independent statistics; nothing synchronizes through them.
    ALIAS_MAPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    ALIAS_PAGES.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
}

/// Why a bound or candidate mapping was not served from the cache. Each
/// distinct reason is reported once per boot (a milestone), so a cache that
/// never hits names its cause without a per-mmap log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Decline {
    /// Registry holds no sealed object, so no attestation was attempted.
    NothingSealed = 0,
    /// vfsd could not attest the descriptor (detail: errno).
    AttestFailed = 1,
    /// vfsd attested a file that matches no sealed object (detail: length).
    NoMatchingObject = 2,
    /// The descriptor carries no usable binding at map time.
    Unbound = 3,
    /// The range reaches past the object's pages.
    OutOfRange = 4,
    /// A page could not be pinned (detail: frame-source error ordinal).
    PinFailed = 5,
    /// The VMA could not be published as a pager VMA (detail: 0 transport, 1 capacity, 2 overlap).
    AdmissionUnavailable = 6,
    /// A demand object could not be created or filled (detail: stage << 32 | code).
    DemandUnavailable = 7,
}

static DECLINES_REPORTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const DECLINE_KINDS: usize = 8;
static DECLINE_COUNT: [core::sync::atomic::AtomicU64; DECLINE_KINDS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; DECLINE_KINDS];
static DECLINE_LAST_DETAIL: [core::sync::atomic::AtomicU64; DECLINE_KINDS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; DECLINE_KINDS];

/// Per-reason `(count, last detail)` since boot, for the probe diagnostic.
pub(crate) fn decline_counters() -> ([u64; DECLINE_KINDS], [u64; DECLINE_KINDS]) {
    let mut counts = [0; DECLINE_KINDS];
    let mut details = [0; DECLINE_KINDS];
    for index in 0..DECLINE_KINDS {
        counts[index] = DECLINE_COUNT[index].load(core::sync::atomic::Ordering::Relaxed);
        details[index] = DECLINE_LAST_DETAIL[index].load(core::sync::atomic::Ordering::Relaxed);
    }
    (counts, details)
}

pub(crate) fn note_decline(reason: Decline, detail: u64) {
    // ORDERING: statistics only; the detail is the latest writer's.
    DECLINE_COUNT[reason as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    DECLINE_LAST_DETAIL[reason as usize].store(detail, core::sync::atomic::Ordering::Relaxed);
    let bit = 1_u32 << reason as u8;
    // ORDERING: elects exactly one reporter per reason; nothing else rides on it.
    if DECLINES_REPORTED.fetch_or(bit, core::sync::atomic::Ordering::Relaxed) & bit == 0 {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "library-alias-declined",
            reason as u64,
            detail,
        );
    }
}

/// `(alias maps, alias pages)` served since boot.
pub(crate) fn alias_counters() -> (u64, u64) {
    (
        ALIAS_MAPS.load(core::sync::atomic::Ordering::Relaxed),
        ALIAS_PAGES.load(core::sync::atomic::Ordering::Relaxed),
    )
}

impl Registry {
    /// `(sealed objects, reserved pages)` for diagnostics.
    pub(crate) fn census(&self) -> (u64, u64) {
        let sealed = self
            .slots
            .iter()
            .flatten()
            .filter(|object| object.state == State::Sealed)
            .count() as u64;
        (sealed, self.reserved_pages)
    }
}

/// Frame-source operations against the real kernel table.
pub(crate) struct KernelSources;
impl PageSources for KernelSources {
    type Pin = kernel_mm::api::frame_source::FrameSourcePin;
    fn supply(
        &self,
        owner: FrameSourceOwner,
        key: FrameSourceKey,
        group: usize,
        bytes: &[u8],
    ) -> Result<u64, FrameSourceError> {
        kernel_mm::api::frame_source::supply_frame_source(
            owner,
            key,
            u64::from(rustos_user_abi::pager::VM_PROT_READ | rustos_user_abi::pager::VM_PROT_EXECUTE),
            group,
            bytes,
        )
    }
    fn pin(
        &self,
        token: u64,
        owner: FrameSourceOwner,
        key: FrameSourceKey,
    ) -> Result<Self::Pin, FrameSourceError> {
        kernel_mm::api::frame_source::pin_frame_source(
            token,
            owner,
            key,
            u64::from(rustos_user_abi::pager::VM_PROT_READ),
        )
    }
    fn retire(&self, token: u64, owner: FrameSourceOwner) {
        let _ = kernel_mm::api::frame_source::retire_frame_source(token, owner);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use rustos_user_abi::syscall::VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// Model of the frame-source table: live tokens keyed by value.
    #[derive(Default)]
    struct Model {
        live: RefCell<BTreeMap<u64, (FrameSourceOwner, FrameSourceKey, Vec<u8>)>>,
        next: RefCell<u64>,
        fail_supply_after: RefCell<Option<u64>>,
    }
    struct ModelPin;
    impl PageSources for Model {
        type Pin = ModelPin;
        fn supply(
            &self,
            owner: FrameSourceOwner,
            key: FrameSourceKey,
            _group: usize,
            bytes: &[u8],
        ) -> Result<u64, FrameSourceError> {
            if let Some(limit) = *self.fail_supply_after.borrow() {
                if self.live.borrow().len() as u64 >= limit {
                    return Err(FrameSourceError::Quota);
                }
            }
            let mut page = bytes.to_vec();
            page.resize(4096, 0);
            *self.next.borrow_mut() += 1;
            let token = *self.next.borrow();
            self.live.borrow_mut().insert(token, (owner, key, page));
            Ok(token)
        }
        fn pin(
            &self,
            token: u64,
            owner: FrameSourceOwner,
            key: FrameSourceKey,
        ) -> Result<ModelPin, FrameSourceError> {
            match self.live.borrow().get(&token) {
                None => Err(FrameSourceError::Stale),
                Some((o, _, _)) if *o != owner => Err(FrameSourceError::Authority),
                Some((_, k, _)) if *k != key => Err(FrameSourceError::Mismatch),
                Some(_) => Ok(ModelPin),
            }
        }
        fn retire(&self, token: u64, owner: FrameSourceOwner) {
            let mut live = self.live.borrow_mut();
            if live.get(&token).is_some_and(|(o, _, _)| *o == owner) {
                live.remove(&token);
            }
        }
    }

    fn owner(generation: u64) -> FrameSourceOwner {
        FrameSourceOwner { service_slot: 3, service_generation: generation }
    }
    fn file(object: u64, len: u64) -> VfsFileIdentityWire {
        VfsFileIdentityWire {
            volume: 1,
            object,
            generation: 4,
            storage_epoch: 6,
            mount_generation: 2,
            len,
            flags: VFS_FILE_IDENTITY_FLAG_IMMUTABLE_VOLUME,
        }
    }
    fn identity(object: u64, len: u64) -> LibraryIdentityWire {
        LibraryIdentityWire { file: file(object, len), content_hash: [9; 32] }
    }
    fn fill(registry: &mut Registry, model: &Model, handle: u64, o: FrameSourceOwner, len: u64) {
        let pages = len.div_ceil(4096);
        for page in 0..pages {
            let bytes = std::vec![page as u8 + 1; ((len - page * 4096).min(4096)) as usize];
            registry.supply_page(model, handle, o, page, 0, &bytes).unwrap();
        }
    }

    #[test]
    fn an_object_serves_only_after_every_page_is_supplied_and_sealed() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, 0, identity(7, 5000)).unwrap();
        assert!(registry.find(&file(7, 5000)).is_none(), "building objects are invisible");
        assert!(!registry.has_sealed_objects());
        registry.supply_page(&model, handle, o, 0, 0, &[1; 4096]).unwrap();
        assert_eq!(registry.seal(handle, o), Err(LibraryError::Incomplete));
        registry.supply_page(&model, handle, o, 1, 0, &[2; 904]).unwrap();
        registry.seal(handle, o).unwrap();
        let found = registry.find(&file(7, 5000)).unwrap();
        assert_eq!(found.handle, handle);
        assert_eq!(found.len, 5000);
        assert!(registry.has_sealed_objects());
        assert_eq!(registry.seal(handle, o), Err(LibraryError::Sealed));
        assert_eq!(
            registry.supply_page(&model, handle, o, 0, 0, &[1; 4096]),
            Err(LibraryError::Sealed)
        );
    }

    #[test]
    fn eof_page_must_carry_exactly_the_bytes_that_exist_and_is_zero_filled() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, 0, identity(7, 4096 + 10)).unwrap();
        registry.supply_page(&model, handle, o, 0, 0, &[1; 4096]).unwrap();
        // Short, long and empty tails are all refused; a pager cannot smuggle
        // bytes past EOF nor leave the page's valid bytes unwritten.
        for wrong in [9_usize, 11, 4096, 0] {
            assert_eq!(
                registry.supply_page(&model, handle, o, 1, 0, &std::vec![5; wrong]),
                Err(LibraryError::Invalid)
            );
        }
        let token = registry.supply_page(&model, handle, o, 1, 0, &[5; 10]).unwrap();
        let live = model.live.borrow();
        let page = &live[&token].2;
        assert_eq!(&page[..10], &[5; 10]);
        assert!(page[10..].iter().all(|byte| *byte == 0));
        assert_eq!(live[&token].1.page_offset, 4096);
    }

    #[test]
    fn page_supply_is_once_only_in_range_and_owner_bound() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, 0, identity(7, 8192)).unwrap();
        registry.supply_page(&model, handle, o, 0, 0, &[1; 4096]).unwrap();
        assert_eq!(registry.supply_page(&model, handle, o, 0, 0, &[1; 4096]), Err(LibraryError::Invalid));
        assert_eq!(registry.supply_page(&model, handle, o, 2, 0, &[1; 4096]), Err(LibraryError::Invalid));
        assert_eq!(
            registry.supply_page(&model, handle, owner(2), 1, 0, &[1; 4096]),
            Err(LibraryError::Authority)
        );
        assert_eq!(registry.seal(handle, owner(2)), Err(LibraryError::Authority));
        assert_eq!(registry.retire(&model, handle, owner(2)), Err(LibraryError::Authority));
    }

    #[test]
    fn identity_is_exact_in_every_field_and_duplicates_are_refused() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, 0, identity(7, 4096)).unwrap();
        assert_eq!(registry.create(o, 0, identity(7, 4096)), Err(LibraryError::Exists));
        fill(&mut registry, &model, handle, o, 4096);
        registry.seal(handle, o).unwrap();
        let exact = file(7, 4096);
        assert!(registry.find(&exact).is_some());
        let variants = [
            VfsFileIdentityWire { volume: 2, ..exact },
            VfsFileIdentityWire { object: 8, ..exact },
            VfsFileIdentityWire { generation: 5, ..exact },
            VfsFileIdentityWire { storage_epoch: 7, ..exact },
            VfsFileIdentityWire { mount_generation: 3, ..exact },
            VfsFileIdentityWire { len: 4095, ..exact },
        ];
        for variant in variants {
            assert!(registry.find(&variant).is_none(), "{variant:?} must not match");
        }
        // A mutable-mount report can never be constructed into an object.
        let mutable = LibraryIdentityWire { file: VfsFileIdentityWire { flags: 0, ..exact }, ..identity(9, 4096) };
        assert_eq!(registry.create(o, 0, mutable), Err(LibraryError::Invalid));
    }

    #[test]
    fn handles_are_generation_exact_and_slots_never_reuse_a_generation() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let first = registry.create(o, 0, identity(7, 4096)).unwrap();
        fill(&mut registry, &model, first, o, 4096);
        registry.seal(first, o).unwrap();
        assert!(registry.view(first).is_ok());
        registry.retire(&model, first, o).unwrap();
        assert_eq!(registry.view(first), Err(LibraryError::Stale));
        let second = registry.create(o, 0, identity(7, 4096)).unwrap();
        assert_ne!(first, second, "the same slot carries a new generation");
        assert_eq!(registry.view(first), Err(LibraryError::Stale));
        assert_eq!(registry.view(second), Err(LibraryError::Stale), "unsealed");
        assert_eq!(registry.view(0), Err(LibraryError::Invalid));
        assert_eq!(registry.view(u64::MAX), Err(LibraryError::Invalid));
    }

    #[test]
    fn pinning_a_range_is_all_or_nothing_and_respects_withdrawal() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, 0, identity(7, 3 * 4096)).unwrap();
        fill(&mut registry, &model, handle, o, 3 * 4096);
        assert_eq!(registry.pin_range(&model, handle, 0, 3).err(), Some(LibraryError::Stale));
        registry.seal(handle, o).unwrap();
        assert_eq!(registry.pin_range(&model, handle, 1, 2).unwrap().len(), 2);
        for (first, count) in [(0, 0), (2, 2), (3, 1), (u64::MAX, 2)] {
            assert_eq!(registry.pin_range(&model, handle, first, count).err(), Some(LibraryError::Invalid));
        }
        // Withdraw one page's source (the pager lease ended mid-life): the
        // range fails as a whole instead of mapping a prefix.
        let token = *model.live.borrow().keys().nth(1).unwrap();
        model.live.borrow_mut().remove(&token);
        assert_eq!(
            registry.pin_range(&model, handle, 0, 3).err(),
            Some(LibraryError::Source(FrameSourceError::Stale))
        );
        assert!(registry.pin_range(&model, handle, 2, 1).is_ok());
    }

    #[test]
    fn retiring_withdraws_every_page_and_returns_the_budget() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, 0, identity(7, 2 * 4096)).unwrap();
        fill(&mut registry, &model, handle, o, 2 * 4096);
        registry.seal(handle, o).unwrap();
        let epoch = registry.epoch();
        assert_eq!(model.live.borrow().len(), 2);
        registry.retire(&model, handle, o).unwrap();
        assert!(model.live.borrow().is_empty());
        assert!(registry.epoch() > epoch, "negative bindings must be revalidated");
        assert!(registry.find(&file(7, 2 * 4096)).is_none());
        assert_eq!(registry.reserved_pages, 0);
        assert_eq!(registry.retire(&model, handle, o), Err(LibraryError::Stale));
    }

    #[test]
    fn a_new_pager_lease_discards_objects_of_the_old_one() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let old = owner(1);
        let handle = registry.create(old, 0, identity(7, 4096)).unwrap();
        fill(&mut registry, &model, handle, old, 4096);
        registry.seal(handle, old).unwrap();
        let fresh = owner(2);
        let replacement = registry.create(fresh, 0, identity(7, 4096)).unwrap();
        assert_ne!(replacement, handle);
        assert_eq!(registry.view(handle), Err(LibraryError::Stale));
        assert_eq!(registry.reserved_pages, 1, "the old object's reservation is released");
    }

    #[test]
    fn registry_bounds_objects_pages_and_quota_failures_publish_nothing() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let mut handles = Vec::new();
        for index in 0..LIBRARY_OBJECT_MAX_OBJECTS as u64 {
            handles.push(registry.create(o, 0, identity(100 + index, 4096)).unwrap());
        }
        assert_eq!(registry.create(o, 0, identity(999, 4096)), Err(LibraryError::NoSlots));
        for handle in handles.drain(..) {
            registry.retire(&model, handle, o).unwrap();
        }
        // Page budget: 12 objects of the full 1024 pages exhaust it.
        for index in 0..(MAX_RESERVED_PAGES / LIBRARY_OBJECT_MAX_PAGES) {
            registry.create(o, 0, identity(200 + index, LIBRARY_OBJECT_MAX_PAGES * 4096)).unwrap();
        }
        assert_eq!(
            registry.create(o, 0, identity(300, LIBRARY_OBJECT_MAX_PAGES * 4096)),
            Err(LibraryError::Budget)
        );
        // A supply the kernel quota refuses records no token and leaves the
        // page suppliable once the pressure clears.
        let mut small = Registry::new();
        let handle = small.create(o, 0, identity(5, 2 * 4096)).unwrap();
        *model.fail_supply_after.borrow_mut() = Some(0);
        assert_eq!(
            small.supply_page(&model, handle, o, 0, 0, &[1; 4096]),
            Err(LibraryError::Source(FrameSourceError::Quota))
        );
        *model.fail_supply_after.borrow_mut() = None;
        assert!(small.supply_page(&model, handle, o, 0, 0, &[1; 4096]).is_ok());
        assert_eq!(small.seal(handle, o), Err(LibraryError::Incomplete));
    }

    #[test]
    fn page_keys_bind_slot_generation_and_offset_and_never_a_path() {
        let first = page_key(0, 1, 0);
        assert_eq!(first.namespace, LIBRARY_OBJECT_BACKING_NAMESPACE);
        assert!(first.is_valid());
        assert_ne!(page_key(0, 1, 1), first);
        assert_ne!(page_key(1, 1, 0), first);
        assert_ne!(page_key(0, 2, 0), first);
        assert_eq!(page_key(0, 1, 3).page_offset, 3 * 4096);
    }

    #[test]
    fn bind_requires_the_exact_attested_file_on_an_immutable_mount() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, 0, identity(7, 5000)).unwrap();
        let exact = file(7, 5000);
        // Unsealed: not eligible, and the verdict names the epoch it saw.
        let early = decide_bind(&registry, &exact, 5000);
        assert!(matches!(early, BindDecision::NotEligible { .. }));
        fill(&mut registry, &model, handle, o, 5000);
        registry.seal(handle, o).unwrap();
        assert_eq!(decide_bind(&registry, &exact, 5000), BindDecision::Bound(handle));
        let epoch = registry.epoch();
        let not_eligible = BindDecision::NotEligible { registry_epoch: epoch };
        // Forged/stale attestations: wrong length, other file, other mount
        // generation, mutable mount, or a guest length vfsd did not attest.
        assert_eq!(decide_bind(&registry, &exact, 4999), not_eligible);
        assert_eq!(decide_bind(&registry, &VfsFileIdentityWire { object: 8, ..exact }, 5000), not_eligible);
        assert_eq!(decide_bind(&registry, &VfsFileIdentityWire { mount_generation: 3, ..exact }, 5000), not_eligible);
        assert_eq!(decide_bind(&registry, &VfsFileIdentityWire { flags: 0, ..exact }, 5000), not_eligible);
        assert_eq!(decide_bind(&registry, &VfsFileIdentityWire { storage_epoch: 0, ..exact }, 5000), not_eligible);
        // After retirement the same attestation no longer binds, and the epoch
        // moved so a cached "not eligible" verdict is revalidated.
        registry.retire(&model, handle, o).unwrap();
        assert!(matches!(decide_bind(&registry, &exact, 5000), BindDecision::NotEligible { registry_epoch } if registry_epoch > epoch));
    }

    fn lib_usage(group: usize) -> u64 {
        kernel_mm::api::kmem_charge::usage(Class::LibraryObject, group)
    }

    #[test]
    fn create_charges_the_owning_job_and_retire_refunds_exactly_once() {
        const G: usize = 52;
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create(o, G, identity(7, 3 * 4096)).unwrap();
        assert_eq!(lib_usage(G), 3 * 8, "one token word per page");
        // Supplying pages allocates no registry heap, so it charges no more.
        fill(&mut registry, &model, handle, o, 3 * 4096);
        registry.seal(handle, o).unwrap();
        assert_eq!(lib_usage(G), 3 * 8);
        registry.retire(&model, handle, o).unwrap();
        assert_eq!(lib_usage(G), 0);
        assert_eq!(registry.retire(&model, handle, o), Err(LibraryError::Stale));
        assert_eq!(lib_usage(G), 0, "a second retire must not refund again");
    }

    #[test]
    fn refused_charge_leaves_no_partial_object() {
        const G: usize = 53;
        let mut registry = Registry::new();
        let o = owner(1);
        kernel_mm::api::kmem_charge::set_limit(Class::LibraryObject, G, 2 * 8);
        assert_eq!(registry.create(o, G, identity(7, 3 * 4096)), Err(LibraryError::Budget));
        assert_eq!(lib_usage(G), 0, "refusal charges nothing");
        assert_eq!(registry.census(), (0, 0), "no reserved pages");
        assert!(registry.slots.iter().all(Option::is_none), "no slot published");
        // The same file fits once the limit allows it; no stale Exists record.
        let handle = registry.create(o, G, identity(8, 2 * 4096)).unwrap();
        assert_eq!(lib_usage(G), 2 * 8);
        registry.retire(&Model::default(), handle, o).unwrap();
        assert_eq!(lib_usage(G), 0);
        kernel_mm::api::kmem_charge::set_limit(Class::LibraryObject, G, kernel_mm::api::charge::UNLIMITED);
    }

    #[test]
    fn replaced_pager_lease_refunds_dropped_objects_once() {
        const G: usize = 54;
        let (mut registry, model) = (Registry::new(), Model::default());
        let old = owner(1);
        registry.create(old, G, identity(7, 4096)).unwrap();
        registry.create(old, G, identity(8, 2 * 4096)).unwrap();
        assert_eq!(lib_usage(G), 3 * 8);
        // A new lease drops the old objects; the charge follows them.
        let fresh = owner(2);
        let handle = registry.create(fresh, G + 1, identity(7, 4096)).unwrap();
        assert_eq!(lib_usage(G), 0);
        assert_eq!(lib_usage(G + 1), 8);
        registry.retire(&model, handle, fresh).unwrap();
        assert_eq!(lib_usage(G + 1), 0);
    }

    fn demand_file(object: u64, len: u64) -> VfsFileIdentityWire {
        file(object, len)
    }
    /// Reads a whole range the way the broker does: claim, supply, repeat.
    fn demand_fill(
        registry: &mut Registry,
        model: &Model,
        handle: u64,
        o: FrameSourceOwner,
        len: u64,
        first: u64,
        end: u64,
    ) {
        let mut from = first;
        loop {
            match registry.claim_gap(model, handle, from, end, 4).unwrap() {
                GapClaim::Done => return,
                GapClaim::Wait { .. } => panic!("no other filler exists"),
                GapClaim::Run { start, pages, resume } => {
                    from = resume;
                    for page in start..start + pages {
                        let bytes = std::vec![(page as u8).wrapping_add(1); ((len - page * 4096).min(4096)) as usize];
                        registry.supply_claimed(model, handle, o, page, 0, &bytes).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn a_demand_object_serves_a_range_only_when_every_page_is_resident() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let len = 100 * 4096 + 10;
        let handle = registry.create_demand(&model, o, 0, &demand_file(7, len)).unwrap();
        assert_eq!(registry.view(handle).unwrap().len, len);
        // Demand objects are found by exact identity but pin nothing until filled.
        assert_eq!(registry.find(&demand_file(7, len)).unwrap().handle, handle);
        assert!(matches!(registry.pin_range(&model, handle, 10, 3), Err(LibraryError::Incomplete)));
        demand_fill(&mut registry, &model, handle, o, len, 10, 13);
        assert_eq!(registry.pin_range(&model, handle, 10, 3).unwrap().len(), 3);
        assert!(matches!(registry.pin_range(&model, handle, 9, 4), Err(LibraryError::Incomplete)));
        assert_eq!(registry.demand_census(), (1, 3), "only the mapped pages are resident");
        // A second range is filled independently; the first stays untouched.
        demand_fill(&mut registry, &model, handle, o, len, 99, 101);
        assert_eq!(registry.demand_census(), (1, 5));
        // The last page carries exactly the bytes that exist and a zero tail.
        let live = model.live.borrow();
        let tail = live.values().find(|(_, key, _)| key.page_offset == 100 * 4096).unwrap();
        assert_eq!(&tail.2[..10], &[101_u8; 10]);
        assert!(tail.2[10..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn concurrent_fillers_never_claim_the_same_page_twice() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let handle = registry.create_demand(&model, o, 0, &demand_file(7, 16 * 4096)).unwrap();
        let first = registry.claim_gap(&model, handle, 0, 16, 8).unwrap();
        assert_eq!(first, GapClaim::Run { start: 0, pages: 8, resume: 0 });
        // The second mapper takes the next run, and once nothing is left to
        // claim it must wait for the first, not read the pages again.
        let second = registry.claim_gap(&model, handle, 0, 16, 8).unwrap();
        assert_eq!(second, GapClaim::Run { start: 8, pages: 8, resume: 0 });
        assert_eq!(registry.claim_gap(&model, handle, 0, 16, 8).unwrap(), GapClaim::Wait { resume: 0 });
        // Claimed pages are not resident: they cannot be pinned or supplied twice.
        assert!(matches!(registry.pin_range(&model, handle, 0, 1), Err(LibraryError::Incomplete)));
        let page = [1_u8; 4096];
        registry.supply_claimed(&model, handle, o, 0, 0, &page).unwrap();
        assert_eq!(registry.supply_claimed(&model, handle, o, 0, 0, &page), Err(LibraryError::Invalid));
        assert_eq!(registry.supply_claimed(&model, handle, o, 17, 0, &page), Err(LibraryError::Invalid));
        // A failed filler gives its claim back and the next mapper retries it.
        registry.unclaim(handle, 8, 8);
        assert_eq!(registry.demand_census().1, 8, "unclaimed pages stop counting as resident");
        assert_eq!(
            registry.claim_gap(&model, handle, 0, 16, 8).unwrap(),
            GapClaim::Run { start: 8, pages: 8, resume: 1 }
        );
        // Unclaim never touches a page that already holds a frame.
        registry.unclaim(handle, 0, 8);
        assert_eq!(registry.claim_gap(&model, handle, 0, 1, 8).unwrap(), GapClaim::Done);
    }

    #[test]
    fn demand_objects_are_exact_immutable_pager_lease_bound_and_never_pager_filled() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let mutable = VfsFileIdentityWire { flags: 0, ..demand_file(7, 4096) };
        assert_eq!(registry.create_demand(&model, o, 0, &mutable), Err(LibraryError::Invalid));
        let too_big = demand_file(8, (LIBRARY_DEMAND_MAX_OBJECT_PAGES + 1) * 4096);
        assert_eq!(registry.create_demand(&model, o, 0, &too_big), Err(LibraryError::Invalid));
        let handle = registry.create_demand(&model, o, 0, &demand_file(7, 4096)).unwrap();
        assert_eq!(registry.create_demand(&model, o, 0, &demand_file(7, 4096)), Err(LibraryError::Exists));
        // Pagerd's warm create cannot shadow the same file either.
        assert_eq!(registry.create(o, 0, identity(7, 4096)), Err(LibraryError::Exists));
        // The pager supply/seal path never reaches a demand object.
        assert_eq!(registry.supply_page(&model, handle, o, 0, 0, &[1; 4096]), Err(LibraryError::Sealed));
        assert_eq!(registry.seal(handle, o), Err(LibraryError::Sealed));
        // A filler from another pager lease is refused.
        registry.claim_gap(&model, handle, 0, 1, 1).unwrap();
        assert_eq!(
            registry.supply_claimed(&model, handle, owner(2), 0, 0, &[1; 4096]),
            Err(LibraryError::Authority)
        );
        // A new lease drops the old lease's demand objects like any other, and
        // the residency they held is returned.
        assert_eq!(registry.demand_census(), (1, 1));
        registry.retire_owner(o);
        assert_eq!(registry.demand_census(), (0, 0));
        assert_eq!(registry.view(handle), Err(LibraryError::Stale));
    }

    #[test]
    fn demand_residency_is_bounded_and_evicts_least_recently_used_objects_first() {
        let (mut registry, model) = (Registry::new(), Model::default());
        registry.demand_resident_limit = 8;
        registry.demand_object_limit = 2;
        let o = owner(1);
        let a = registry.create_demand(&model, o, 0, &demand_file(1, 4 * 4096)).unwrap();
        let b = registry.create_demand(&model, o, 0, &demand_file(2, 4 * 4096)).unwrap();
        demand_fill(&mut registry, &model, a, o, 4 * 4096, 0, 4);
        demand_fill(&mut registry, &model, b, o, 4 * 4096, 0, 4);
        assert_eq!(registry.demand_census(), (2, 8));
        let live_before = model.live.borrow().len();
        // Touch `a` so `b` is the least recently used, then ask for room.
        registry.view(a).unwrap();
        let c = registry.create_demand(&model, o, 0, &demand_file(3, 4 * 4096)).unwrap();
        assert_eq!(registry.view(b), Err(LibraryError::Stale), "LRU object evicted by the object bound");
        assert!(registry.view(a).is_ok());
        assert_eq!(model.live.borrow().len(), live_before - 4, "its frame sources were withdrawn");
        assert_eq!(registry.demand_census(), (2, 4));
        demand_fill(&mut registry, &model, c, o, 4 * 4096, 0, 4);
        // Residency bound: a fourth object's pages evict the older one, never the
        // object being filled.
        registry.touch_for_test(c);
        let d = registry.create_demand(&model, o, 0, &demand_file(4, 4 * 4096)).unwrap();
        assert_eq!(registry.view(a), Err(LibraryError::Stale));
        demand_fill(&mut registry, &model, d, o, 4 * 4096, 0, 4);
        assert_eq!(registry.demand_census(), (2, 8));
        // One object larger than the whole bound cannot be filled.
        let mut tiny = Registry::new();
        tiny.demand_resident_limit = 2;
        let e = tiny.create_demand(&model, o, 0, &demand_file(5, 8 * 4096)).unwrap();
        assert_eq!(tiny.claim_gap(&model, e, 0, 8, 8), Err(LibraryError::Budget));
        assert_eq!(tiny.demand_census(), (1, 0), "a refused claim holds nothing");
    }

    #[test]
    fn demand_token_words_are_allocated_per_mapped_chunk_and_billed() {
        const G: usize = 61;
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        // A 306 MB file: 74,900 pages, 147 chunks of 512 token words.
        let len = 306_789_704;
        let handle = registry.create_demand(&model, o, G, &demand_file(7, len)).unwrap();
        let table = len.div_ceil(4096).div_ceil(512) * core::mem::size_of::<Option<Box<[u64]>>>() as u64;
        assert_eq!(lib_usage(G), table, "only the chunk table exists before any page is claimed");
        demand_fill(&mut registry, &model, handle, o, len, 600, 700);
        assert_eq!(lib_usage(G), table + 4096, "one 512-page chunk covers pages 512..1024");
        demand_fill(&mut registry, &model, handle, o, len, 1000, 1030);
        assert_eq!(lib_usage(G), table + 2 * 4096);
        registry.retire(&model, handle, o).unwrap();
        assert_eq!(lib_usage(G), 0, "retire refunds the table and every chunk");
        assert!(model.live.borrow().is_empty());
    }

    #[test]
    fn a_demand_object_binds_later_mappers_of_the_same_attested_file() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        let exact = demand_file(7, 5000);
        assert!(matches!(decide_bind(&registry, &exact, 5000), BindDecision::NotEligible { .. }));
        let handle = registry.create_demand(&model, o, 0, &exact).unwrap();
        assert_eq!(decide_bind(&registry, &exact, 5000), BindDecision::Bound(handle));
        let epoch = registry.epoch();
        assert_eq!(
            decide_bind(&registry, &VfsFileIdentityWire { generation: 5, ..exact }, 5000),
            BindDecision::NotEligible { registry_epoch: epoch },
            "a different generation of the file is a different file"
        );
        assert!(registry.has_sealed_objects(), "a demand object counts as servable");
    }

    #[test]
    fn create_demand_evicts_lru_when_warm_objects_fill_remaining_slots() {
        let (mut registry, model) = (Registry::new(), Model::default());
        let o = owner(1);
        // Fill all but 2 slots with warm objects so demand_object_count() stays
        // below demand_object_limit (32), yet the slot table itself fills up.
        for index in 0..(LIBRARY_OBJECT_MAX_OBJECTS as u64 - 2) {
            registry.create(o, 0, identity(1000 + index, 4096)).unwrap();
        }
        let first = registry.create_demand(&model, o, 0, &demand_file(1, 4096)).unwrap();
        let second = registry.create_demand(&model, o, 0, &demand_file(2, 4096)).unwrap();
        registry.touch_for_test(second);
        let third = registry.create_demand(&model, o, 0, &demand_file(3, 4096)).unwrap();
        assert_eq!(registry.view(first), Err(LibraryError::Stale), "LRU demand object evicted to free a slot");
        assert!(registry.view(second).is_ok());
        assert!(registry.view(third).is_ok());
    }
}
