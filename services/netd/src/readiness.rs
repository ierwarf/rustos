//! Socket readiness linearization: dirty tracking, observation and generation
//! stamping for every netd socket.
//!
//! - **Owner:** `NetState` owns readiness observations and generations; the
//!   state lock is the linearization point and publication follows unlock.
//! - **Boundary:** Socket tokens and readiness queries come from untrusted
//!   clients; only tokens present in the tables are ever stamped.
//! - **Lifecycle:** Create/mutate marks a token dirty, the guard drop observes
//!   dirty (or all INET) sockets, stamps changes, and publishes after unlock;
//!   final close publishes once and drops the live generation.
//! - **Concurrency:** Observation and stamping run under the single state
//!   mutex; the global generation is monotonic and wakes accept only newer
//!   generations, so concurrent publishers cannot downgrade a waiter.
//! - **Failure:** Generation exhaustion exits the service for rootd restart;
//!   a missed transition (smoltcp reports none for timer-only changes) is
//!   bounded by the full sweep, requested on any reported change and at least
//!   every `INET_TIMER_SWEEP_INTERVAL`.
//! - **Forbidden:** No readiness change without a generation bump, no
//!   publication under the state lock, no silent dirty-set truncation.
//! - **Evidence:** `readiness_tests`, `local_socket_poll_tests`, and
//!   `endpoint-receiver-wakeup`.

use super::*;
use std::collections::BTreeSet;
use std::ops::{Deref, DerefMut};

pub(super) struct SocketTable<T> {
    entries: BTreeMap<u64, T>,
    dirty: BTreeSet<u64>,
    /// Every identity currently in `entries` is dirty. Kept as a flag so a
    /// group dirtying costs O(1) instead of one set insertion per socket;
    /// retired identities are still recorded individually in `dirty`.
    all_dirty: bool,
}

impl<T> SocketTable<T> {
    pub(super) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            dirty: BTreeSet::new(),
            all_dirty: false,
        }
    }

    pub(super) fn insert(&mut self, token: u64, socket: T) -> Option<T> {
        self.dirty.insert(token);
        self.entries.insert(token, socket)
    }

    pub(super) fn remove(&mut self, token: &u64) -> Option<T> {
        self.dirty.insert(*token);
        self.entries.remove(token)
    }

    pub(super) fn get_mut(&mut self, token: &u64) -> Option<&mut T> {
        self.dirty.insert(*token);
        self.entries.get_mut(token)
    }

    pub(super) fn values_mut(&mut self) -> std::collections::btree_map::ValuesMut<'_, u64, T> {
        self.mark_all_dirty();
        self.entries.values_mut()
    }

    fn mark_all_dirty(&mut self) {
        self.all_dirty = true;
    }

    fn has_dirty(&self) -> bool {
        self.all_dirty || !self.dirty.is_empty()
    }
}

// Deliberately no DerefMut: every mutation must retain its dirty identity.
impl<T> Deref for SocketTable<T> {
    type Target = BTreeMap<u64, T>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Readiness {
    Unix { events: u32, peer: Option<u64> },
    Inet(InetReadiness),
}

/// Records `after` as the token's observation; returns whether it changed.
/// One map descent per token, and no write when nothing changed.
fn observe(
    observations: &mut BTreeMap<u64, Readiness>,
    token: u64,
    after: Option<Readiness>,
) -> bool {
    use std::collections::btree_map::Entry;
    match (observations.entry(token), after) {
        (Entry::Occupied(mut slot), Some(after)) => {
            if *slot.get() == after {
                false
            } else {
                slot.insert(after);
                true
            }
        }
        (Entry::Occupied(slot), None) => {
            slot.remove();
            true
        }
        (Entry::Vacant(slot), Some(after)) => {
            slot.insert(after);
            true
        }
        (Entry::Vacant(_), None) => false,
    }
}

impl NetState {
    pub(super) fn record_readiness_changes(&mut self) {
        let stack_advanced = self
            .inet
            .as_mut()
            .is_some_and(|stack| std::mem::take(&mut stack.readiness_dirty));
        let mut changed = Vec::new();
        if self.sockets.has_dirty() {
            self.observe_unix(&mut changed);
        }
        if stack_advanced || self.inet_sockets.has_dirty() {
            self.observe_inet(stack_advanced, &mut changed);
        }
        if changed.is_empty() {
            return;
        }
        // Publication order stays ascending by token across both tables.
        changed.sort_unstable();
        let generation = READINESS_GENERATION
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(1)
            })
            .unwrap_or_else(|_| {
                debug_line("netd: readiness generation exhausted");
                std::process::exit(134);
            })
            + 1;
        for token in changed {
            if self.socket_exists(token) {
                self.readiness_generations.insert(token, generation);
            } else {
                self.readiness_generations.remove(&token);
            }
            // Include final close, even though its live generation is gone.
            self.readiness_publications.push((token, generation));
        }
    }

    /// Ordinary Unix traffic visits only dirty Unix identities, never the
    /// whole table.
    fn observe_unix(&mut self, changed: &mut Vec<u64>) {
        let mut dirty = std::mem::take(&mut self.sockets.dirty);
        self.sockets.all_dirty = false;
        // Unix writability depends on the peer's receive capacity. Recheck
        // both the old and current peer, including a just-retired endpoint.
        let touched: Vec<_> = dirty.iter().copied().collect();
        for token in touched {
            if let Some(UnixSocket {
                state: UnixSocketState::Connected(connected),
                ..
            }) = self.sockets.get(&token)
            {
                dirty.insert(connected.peer);
            }
            if let Some(Readiness::Unix {
                peer: Some(peer), ..
            }) = self.readiness_observations.get(&token)
            {
                dirty.insert(*peer);
            }
        }
        for token in dirty {
            let after = if self.sockets.contains_key(&token) {
                const EVENTS: u32 =
                    (linux_abi::POLLIN | linux_abi::POLLOUT | linux_abi::POLLRDHUP) as u32;
                let peer = match &self.sockets[&token].state {
                    UnixSocketState::Connected(connected) => Some(connected.peer),
                    _ => None,
                };
                Some(Readiness::Unix {
                    events: unix_socket_revents(self, token, EVENTS).expect("live Unix socket"),
                    peer,
                })
            } else {
                // Retired: the identity is gone from both tables.
                None
            };
            if observe(&mut self.readiness_observations, token, after) {
                changed.push(token);
            }
        }
    }

    /// Stages and compares INET sockets. Cross-socket ICMP errors are staged
    /// before any comparison. smoltcp advances the shared socket set, so a
    /// stack advance (or an error packet touching an arbitrary socket) must
    /// examine every INET socket; a request that only touched named sockets
    /// examines just those, because every other smoltcp-visible mutation is
    /// followed by a stack advance in the same request.
    fn observe_inet(&mut self, stack_advanced: bool, changed: &mut Vec<u64>) {
        let NetState {
            inet,
            inet_sockets,
            readiness_observations,
            ..
        } = self;
        if let Some(stack) = inet.as_mut() {
            inet_datagram::stage_errors(stack, inet_sockets);
        }
        let all = stack_advanced || inet_sockets.all_dirty;
        inet_sockets.all_dirty = false;
        let dirty = std::mem::take(&mut inet_sockets.dirty);
        let mut visit = |token: u64, socket: Option<&mut InetSocket>| {
            let after = match (inet.as_mut(), socket) {
                (Some(stack), Some(socket)) => {
                    inet_datagram::stage_socket(stack, socket);
                    Some(Readiness::Inet(inet_datagram::readiness(stack, socket)))
                }
                _ => None,
            };
            if observe(readiness_observations, token, after) {
                changed.push(token);
            }
        };
        if all {
            for (&token, socket) in inet_sockets.entries.iter_mut() {
                visit(token, Some(socket));
            }
            // Retired identities are no longer in the table.
            for token in dirty {
                if !inet_sockets.entries.contains_key(&token) {
                    visit(token, None);
                }
            }
        } else {
            for token in dirty {
                visit(token, inet_sockets.entries.get_mut(&token));
            }
        }
    }
}

pub(super) struct NetStateGuard<'a> {
    state: Option<MutexGuard<'a, NetState>>,
}

impl<'a> NetStateGuard<'a> {
    pub(super) fn new(state: MutexGuard<'a, NetState>) -> Self {
        Self { state: Some(state) }
    }
}

impl Deref for NetStateGuard<'_> {
    type Target = NetState;
    fn deref(&self) -> &NetState {
        self.state.as_deref().expect("live network guard")
    }
}

impl DerefMut for NetStateGuard<'_> {
    fn deref_mut(&mut self) -> &mut NetState {
        self.state.as_deref_mut().expect("live network guard")
    }
}

impl Drop for NetStateGuard<'_> {
    fn drop(&mut self) {
        self.record_readiness_changes();
        let publications = std::mem::take(&mut self.readiness_publications);
        drop(self.state.take());
        // Broker wake matching accepts only strictly newer generations, so
        // concurrent publishers may arrive out of order without a downgrade.
        for (token, generation) in publications {
            publish_readiness_generation(token, generation);
        }
    }
}

pub(super) fn publish_readiness_generation(object_id: u64, generation: u64) {
    #[cfg(test)]
    readiness::record_test_publication(object_id, generation);
    #[cfg(not(test))]
    {
        let args = WaitSetSignalBrokerArgs {
            abi_version: WAITSET_ABI_VERSION,
            provider: WAITSET_PROVIDER_NETD,
            flags: 0,
            object_id,
            generation,
            reserved0: 0,
        };
        let result = syscall1(
            SYS_RUSTOS_WAITSET_SIGNAL_BROKER,
            (&args as *const WaitSetSignalBrokerArgs) as u64,
        );
        if result < 0 {
            debug_line("netd: readiness generation publication failed");
            std::process::exit(134);
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PUBLICATIONS: std::cell::RefCell<Vec<(u64, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(super) fn record_test_publication(token: u64, generation: u64) {
    TEST_PUBLICATIONS.with(|publications| publications.borrow_mut().push((token, generation)));
}

#[cfg(test)]
#[path = "tests/readiness_tests.rs"]
mod tests;
