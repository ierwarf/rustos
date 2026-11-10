//! Typed startup and readiness state machine for uiserver.
//!
//! uiserver depends on six things that come up on independent schedules: the
//! UI assets, the display provider, the input consumer, the Wayland socket,
//! the runtime broker and the GPU compositor. Each one is a [`Dependency`]
//! with an owned [`RetryPolicy`] (bounded attempts, bounded total budget,
//! exponential backoff). The machine is pure: every method takes the time as a
//! `Duration` since process start, so slow-dependency and lost-edge behaviour
//! is tested without sleeping.
//!
//! [`Startup`] wraps the machine for the real process. It is the one place that
//! decides "ready": the canonical `uiserver: readiness v1 ...` line is printed
//! from it on every transition and the harness (and `runtimed`) read that line
//! instead of inferring readiness from unrelated log text. Design, prior art
//! and invariants: `docs/ai/ui-startup-readiness.md`.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

pub(crate) const READINESS_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dependency {
    Assets,
    Display,
    Input,
    Wayland,
    Runtime,
    Gpu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Criticality {
    /// The desktop is not ready without it, and an exhausted budget is a
    /// service failure.
    Required,
    /// The desktop runs without it (bundled fallback); exhaustion degrades.
    Optional,
}

impl Dependency {
    pub(crate) const ALL: [Dependency; 6] = [
        Dependency::Assets,
        Dependency::Display,
        Dependency::Input,
        Dependency::Wayland,
        Dependency::Runtime,
        Dependency::Gpu,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Dependency::Assets => "assets",
            Dependency::Display => "display",
            Dependency::Input => "input",
            Dependency::Wayland => "wayland",
            Dependency::Runtime => "runtime",
            Dependency::Gpu => "gpu",
        }
    }

    pub(crate) const fn criticality(self) -> Criticality {
        match self {
            Dependency::Assets => Criticality::Optional,
            _ => Criticality::Required,
        }
    }

    /// Each dependency owns its budget. The budgets are deliberately larger
    /// than one worst-case IPC call (`IPC_BOOT_CONTROL_HARD_LIMIT_MS`, 5 s):
    /// under CPU contention a single attempt can consume a whole call deadline,
    /// so a budget of two calls leaves no retry at all.
    pub(crate) const fn policy(self) -> RetryPolicy {
        match self {
            Dependency::Assets => RetryPolicy::new(ms(250), ms(1_000), secs(120), 0),
            Dependency::Display => RetryPolicy::new(ms(25), ms(250), secs(30), 0),
            Dependency::Input => RetryPolicy::new(ms(50), ms(500), secs(45), 0),
            Dependency::Runtime => RetryPolicy::new(ms(100), ms(1_000), secs(120), 0),
            // Wayland and the GPU compositor own their internal machinery and
            // are observed here: the machine records the outcome and the
            // deadline, but never re-drives them.
            Dependency::Wayland | Dependency::Gpu => RetryPolicy::observe(secs(45)),
        }
    }
}

const fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

const fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

/// Bounded retry schedule. `max_attempts == 0` means the total budget is the
/// only attempt bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
    pub(crate) total_budget: Duration,
    pub(crate) max_attempts: u32,
    /// Observed dependencies are never retried by the machine.
    pub(crate) observed_only: bool,
}

impl RetryPolicy {
    pub(crate) const fn new(
        initial_backoff: Duration,
        max_backoff: Duration,
        total_budget: Duration,
        max_attempts: u32,
    ) -> Self {
        Self {
            initial_backoff,
            max_backoff,
            total_budget,
            max_attempts,
            observed_only: false,
        }
    }

    pub(crate) const fn observe(total_budget: Duration) -> Self {
        Self {
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            total_budget,
            max_attempts: 1,
            observed_only: true,
        }
    }

    /// Delay before attempt `failed_attempts + 1`: `initial * 2^(n-1)`, capped.
    pub(crate) fn backoff_after(&self, failed_attempts: u32) -> Duration {
        let shift = failed_attempts.saturating_sub(1).min(16);
        self.initial_backoff
            .saturating_mul(1_u32 << shift)
            .min(self.max_backoff)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DepState {
    /// Nothing has started; the startup epoch still bounds this state.
    Pending,
    Attempting {
        attempt: u32,
        since: Duration,
    },
    Backoff {
        attempt: u32,
        retry_at: Duration,
        errno: i32,
    },
    Ready {
        attempts: u32,
        at: Duration,
    },
    /// Terminal. The reason is the last errno (or `ETIMEDOUT` when the budget
    /// expired with an attempt that never reported).
    Failed {
        attempts: u32,
        errno: i32,
        at: Duration,
    },
}

impl DepState {
    const fn label(&self) -> &'static str {
        match self {
            DepState::Pending => "pending",
            DepState::Attempting { .. } => "attempting",
            DepState::Backoff { .. } => "backoff",
            DepState::Ready { .. } => "ready",
            DepState::Failed { .. } => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    Starting,
    /// Every required dependency is ready and every optional one is ready.
    Ready,
    /// Every required dependency is ready; an optional one failed.
    Degraded,
    /// A required dependency exhausted its budget. The service must exit with
    /// this errno so the supervisor sees a typed failure.
    Failed(Dependency, i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    RetryAfter(Duration),
    Exhausted { errno: i32 },
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    state: DepState,
    attempts: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct StartupMachine {
    slots: [Slot; 6],
    generation: u64,
}

impl Default for StartupMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StartupMachine {
    pub(crate) fn new() -> Self {
        Self {
            slots: [Slot {
                state: DepState::Pending,
                attempts: 0,
            }; 6],
            generation: 0,
        }
    }

    pub(crate) fn state(&self, dep: Dependency) -> DepState {
        self.slots[dep.index()].state
    }

    /// Increments on every state change. A reader that saw generation `g` and
    /// reads `g` again knows nothing changed in between.
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    fn set(&mut self, dep: Dependency, state: DepState) {
        self.slots[dep.index()].state = state;
        self.generation = self.generation.wrapping_add(1).max(1);
    }

    /// Start (or restart after a backoff) one attempt. Returns the attempt
    /// number, or `None` when the dependency is already terminal.
    pub(crate) fn begin_attempt(&mut self, dep: Dependency, now: Duration) -> Option<u32> {
        self.expire(now);
        let slot = &mut self.slots[dep.index()];
        if matches!(slot.state, DepState::Ready { .. } | DepState::Failed { .. }) {
            return None;
        }
        slot.attempts = slot.attempts.saturating_add(1);
        let attempt = slot.attempts;
        self.set(
            dep,
            DepState::Attempting {
                attempt,
                since: now,
            },
        );
        Some(attempt)
    }

    /// Record success. A late success after the budget already failed the
    /// dependency is refused: `Failed` is terminal so a published failure is
    /// never contradicted. Returns whether the state changed.
    pub(crate) fn succeed(&mut self, dep: Dependency, now: Duration) -> bool {
        self.expire(now);
        let slot = &mut self.slots[dep.index()];
        match slot.state {
            DepState::Ready { .. } | DepState::Failed { .. } => false,
            _ => {
                let attempts = slot.attempts.max(1);
                self.set(dep, DepState::Ready { attempts, at: now });
                true
            }
        }
    }

    /// Record a failed attempt and decide what happens next.
    pub(crate) fn fail_attempt(
        &mut self,
        dep: Dependency,
        errno: i32,
        retryable: bool,
        now: Duration,
    ) -> Next {
        self.expire(now);
        let policy = dep.policy();
        let slot = self.slots[dep.index()];
        if let DepState::Failed { errno, .. } = slot.state {
            return Next::Exhausted { errno };
        }
        if matches!(slot.state, DepState::Ready { .. }) {
            return Next::Exhausted { errno };
        }
        let attempts = slot.attempts.max(1);
        let spent = now;
        let backoff = policy.backoff_after(attempts);
        let attempts_left = policy.max_attempts == 0 || attempts < policy.max_attempts;
        // The retry has to *finish its wait* inside the budget; otherwise the
        // failure would be published only after the deadline it owns.
        let within_budget = spent.saturating_add(backoff) < policy.total_budget;
        if retryable && !policy.observed_only && attempts_left && within_budget {
            self.set(
                dep,
                DepState::Backoff {
                    attempt: attempts,
                    retry_at: now.saturating_add(backoff),
                    errno,
                },
            );
            Next::RetryAfter(backoff)
        } else {
            self.set(
                dep,
                DepState::Failed {
                    attempts,
                    errno,
                    at: now,
                },
            );
            Next::Exhausted { errno }
        }
    }

    /// Fail every dependency whose budget expired with no report. This is the
    /// lost-edge guard: a worker that died, or a call that never returns, must
    /// not leave a dependency `Attempting` forever. Returns the dependencies
    /// failed by this call.
    pub(crate) fn expire(&mut self, now: Duration) -> Vec<Dependency> {
        let mut expired = Vec::new();
        for dep in Dependency::ALL {
            let slot = self.slots[dep.index()];
            if !matches!(
                slot.state,
                DepState::Pending | DepState::Attempting { .. } | DepState::Backoff { .. }
            ) {
                continue;
            }
            if now >= dep.policy().total_budget {
                self.set(
                    dep,
                    DepState::Failed {
                        attempts: slot.attempts,
                        errno: libc::ETIMEDOUT,
                        at: now,
                    },
                );
                expired.push(dep);
            }
        }
        expired
    }

    pub(crate) fn readiness(&self) -> Readiness {
        let mut starting = false;
        let mut degraded = false;
        for dep in Dependency::ALL {
            match (self.state(dep), dep.criticality()) {
                (DepState::Ready { .. }, _) => {}
                (DepState::Failed { errno, .. }, Criticality::Required) => {
                    return Readiness::Failed(dep, errno)
                }
                (DepState::Failed { .. }, Criticality::Optional) => degraded = true,
                _ => starting = true,
            }
        }
        match (starting, degraded) {
            (true, _) => Readiness::Starting,
            (false, true) => Readiness::Degraded,
            (false, false) => Readiness::Ready,
        }
    }

    /// The canonical readiness record: one line, fixed key order, no free text.
    pub(crate) fn report(&self) -> String {
        let (state, cause) = match self.readiness() {
            Readiness::Starting => ("starting", String::new()),
            Readiness::Ready => ("ready", String::new()),
            Readiness::Degraded => ("degraded", String::new()),
            Readiness::Failed(dep, errno) => {
                ("failed", format!(" cause={} errno={errno}", dep.name()))
            }
        };
        let mut line = format!(
            "uiserver: readiness v{READINESS_PROTOCOL_VERSION} state={state} gen={}{cause}",
            self.generation
        );
        for dep in Dependency::ALL {
            let slot = self.slots[dep.index()];
            line.push(' ');
            line.push_str(dep.name());
            line.push('=');
            line.push_str(slot.state.label());
            match slot.state {
                DepState::Attempting { attempt, .. } | DepState::Backoff { attempt, .. } => {
                    line.push_str(&format!("#{attempt}"));
                }
                DepState::Failed {
                    errno, attempts, ..
                } => {
                    line.push_str(&format!("#{attempts}/errno{errno}"));
                }
                _ => {}
            }
        }
        line
    }
}

/// Process-wide readiness authority.
pub(crate) struct Startup {
    machine: Mutex<StartupMachine>,
    epoch: Instant,
}

static STARTUP: OnceLock<Startup> = OnceLock::new();

pub(crate) fn global() -> &'static Startup {
    STARTUP.get_or_init(|| Startup {
        machine: Mutex::new(StartupMachine::new()),
        epoch: Instant::now(),
    })
}

impl Startup {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn lock(&self) -> MutexGuard<'_, StartupMachine> {
        // The machine is plain data replaced under the lock; a poisoned guard
        // still holds a consistent snapshot.
        self.machine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, line: &str) {
        crate::sys::debug_line(line);
        crate::sys::diag_line(line);
    }

    pub(crate) fn begin(&self, dep: Dependency) -> Option<u32> {
        let mut machine = self.lock();
        let now = self.now();
        let generation = machine.generation;
        let attempt = machine.begin_attempt(dep, now);
        // Beginning an attempt may also expire another pending dependency.
        if machine.generation != generation {
            let line = machine.report();
            drop(machine);
            self.publish(&line);
        }
        attempt
    }

    pub(crate) fn succeed(&self, dep: Dependency) {
        let mut machine = self.lock();
        let now = self.now();
        let generation = machine.generation;
        let changed = machine.succeed(dep, now);
        if changed || machine.generation != generation {
            let line = machine.report();
            drop(machine);
            self.publish(&line);
        }
    }

    pub(crate) fn fail(&self, dep: Dependency, errno: i32, retryable: bool) -> Next {
        let mut machine = self.lock();
        let now = self.now();
        let next = machine.fail_attempt(dep, errno, retryable, now);
        let line = machine.report();
        drop(machine);
        self.publish(&line);
        next
    }

    /// Run the expiry guard and return the typed failure, if any. Called from
    /// the UI loop so a dependency whose worker vanished surfaces as a
    /// published failure instead of a silent hang.
    pub(crate) fn poll_failure(&self) -> Option<(Dependency, i32)> {
        let mut machine = self.lock();
        let now = self.now();
        let expired = machine.expire(now);
        let failure = match machine.readiness() {
            Readiness::Failed(dep, errno) => Some((dep, errno)),
            _ => None,
        };
        if !expired.is_empty() {
            let line = machine.report();
            drop(machine);
            self.publish(&line);
        }
        failure
    }

    /// Drive `operation` to success under the dependency's policy. Blocks the
    /// calling thread for backoff; never call it from the UI loop.
    pub(crate) fn drive<T>(
        &self,
        dep: Dependency,
        retryable: impl Fn(i32) -> bool,
        mut operation: impl FnMut() -> Result<T, i32>,
    ) -> Result<T, i32> {
        loop {
            if self.begin(dep).is_none() {
                // Already terminal (for example expired while we were blocked).
                return Err(match self.lock().state(dep) {
                    DepState::Failed { errno, .. } => errno,
                    _ => libc::ECANCELED,
                });
            }
            match operation() {
                Ok(value) => {
                    let mut machine = self.lock();
                    let expired = machine.expire(self.now());
                    let failure = match machine.state(dep) {
                        DepState::Failed { errno, .. } => Some(errno),
                        _ => None,
                    };
                    if !expired.is_empty() {
                        let line = machine.report();
                        drop(machine);
                        self.publish(&line);
                    }
                    return match failure {
                        Some(errno) => Err(errno),
                        None => Ok(value),
                    };
                }
                Err(errno) => match self.fail(dep, errno, retryable(errno)) {
                    Next::RetryAfter(delay) => std::thread::sleep(delay),
                    Next::Exhausted { errno } => return Err(errno),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    #[test]
    fn readiness_requires_every_dependency() {
        let mut machine = StartupMachine::new();
        assert_eq!(machine.readiness(), Readiness::Starting);
        for dep in Dependency::ALL {
            assert_eq!(machine.readiness(), Readiness::Starting, "before {dep:?}");
            machine.begin_attempt(dep, t(1));
            machine.succeed(dep, t(2));
        }
        assert_eq!(machine.readiness(), Readiness::Ready);
        assert!(machine
            .report()
            .starts_with("uiserver: readiness v1 state=ready "));
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let policy = RetryPolicy::new(t(100), t(500), secs(60), 0);
        assert_eq!(policy.backoff_after(1), t(100));
        assert_eq!(policy.backoff_after(2), t(200));
        assert_eq!(policy.backoff_after(3), t(400));
        assert_eq!(policy.backoff_after(4), t(500));
        assert_eq!(policy.backoff_after(60), t(500));
    }

    #[test]
    fn a_slow_dependency_is_retried_until_its_budget_not_its_first_timeout() {
        // Each attempt blocks for a full 5 s IPC deadline and fails with
        // ETIMEDOUT. The input budget (45 s) must allow several attempts; the
        // old 10 s budget allowed two and then killed the service.
        let mut machine = StartupMachine::new();
        for dep in Dependency::ALL {
            if dep != Dependency::Input {
                machine.succeed(dep, t(0));
            }
        }
        let mut now = t(0);
        let mut attempts = 0;
        loop {
            machine.begin_attempt(Dependency::Input, now).unwrap();
            attempts += 1;
            now += secs(5);
            match machine.fail_attempt(Dependency::Input, libc::ETIMEDOUT, true, now) {
                Next::RetryAfter(delay) => now += delay,
                Next::Exhausted { errno } => {
                    assert_eq!(errno, libc::ETIMEDOUT);
                    break;
                }
            }
        }
        assert!(attempts >= 6, "attempts={attempts}");
        assert!(
            now <= secs(45) + secs(5),
            "failure published after the owned deadline"
        );
        assert_eq!(
            machine.readiness(),
            Readiness::Failed(Dependency::Input, libc::ETIMEDOUT)
        );
    }

    #[test]
    fn a_nonretryable_error_fails_immediately_with_its_errno() {
        let mut machine = StartupMachine::new();
        machine.begin_attempt(Dependency::Display, t(0));
        let next = machine.fail_attempt(Dependency::Display, libc::EINVAL, false, t(1));
        assert_eq!(
            next,
            Next::Exhausted {
                errno: libc::EINVAL
            }
        );
        assert_eq!(
            machine.readiness(),
            Readiness::Failed(Dependency::Display, libc::EINVAL)
        );
    }

    #[test]
    fn a_failed_dependency_never_becomes_ready_late() {
        let mut machine = StartupMachine::new();
        machine.begin_attempt(Dependency::Runtime, t(0));
        machine.fail_attempt(Dependency::Runtime, libc::EIO, false, t(1));
        let generation = machine.generation();
        assert!(!machine.succeed(Dependency::Runtime, t(2)));
        assert!(machine.begin_attempt(Dependency::Runtime, t(3)).is_none());
        assert_eq!(machine.generation(), generation);
        assert!(matches!(
            machine.state(Dependency::Runtime),
            DepState::Failed { .. }
        ));
    }

    #[test]
    fn a_lost_completion_edge_is_expired_by_the_budget() {
        // The worker began an attempt and never reported (died or blocked
        // forever). Nothing else would ever move this dependency.
        let mut machine = StartupMachine::new();
        for dep in Dependency::ALL {
            if dep != Dependency::Runtime {
                machine.succeed(dep, t(0));
            }
        }
        machine.begin_attempt(Dependency::Runtime, t(0));
        assert!(machine.expire(secs(119)).is_empty());
        assert_eq!(machine.readiness(), Readiness::Starting);
        assert_eq!(machine.expire(secs(120)), vec![Dependency::Runtime]);
        assert_eq!(
            machine.readiness(),
            Readiness::Failed(Dependency::Runtime, libc::ETIMEDOUT)
        );
    }

    #[test]
    fn a_dependency_that_never_started_expires_from_startup_epoch() {
        let mut machine = StartupMachine::new();
        assert!(machine.expire(secs(29)).is_empty());
        assert_eq!(machine.expire(secs(30)), vec![Dependency::Display]);
        assert_eq!(
            machine.readiness(),
            Readiness::Failed(Dependency::Display, libc::ETIMEDOUT)
        );
        assert!(machine
            .begin_attempt(Dependency::Display, secs(31))
            .is_none());
    }

    #[test]
    fn success_at_deadline_cannot_revive_unreported_dependency() {
        let mut machine = StartupMachine::new();
        machine.begin_attempt(Dependency::Input, t(0));
        assert!(!machine.succeed(Dependency::Input, secs(45)));
        assert_eq!(
            machine.state(Dependency::Input),
            DepState::Failed {
                attempts: 1,
                errno: libc::ETIMEDOUT,
                at: secs(45),
            }
        );
    }

    #[test]
    fn an_optional_failure_degrades_instead_of_failing() {
        let mut machine2 = StartupMachine::new();
        for dep in Dependency::ALL {
            machine2.begin_attempt(dep, t(0));
            if dep == Dependency::Assets {
                machine2.fail_attempt(dep, libc::EINVAL, false, t(1));
            } else {
                machine2.succeed(dep, t(1));
            }
        }
        assert_eq!(machine2.readiness(), Readiness::Degraded);
        assert!(machine2.report().contains(" assets=failed#1/errno22"));
    }

    #[test]
    fn observed_dependencies_are_never_redriven() {
        let mut machine = StartupMachine::new();
        machine.begin_attempt(Dependency::Gpu, t(0));
        assert_eq!(
            machine.fail_attempt(Dependency::Gpu, libc::ENODEV, true, t(1)),
            Next::Exhausted {
                errno: libc::ENODEV
            }
        );
    }

    #[test]
    fn every_transition_advances_the_generation_exactly_once() {
        let mut machine = StartupMachine::new();
        let g0 = machine.generation();
        machine.begin_attempt(Dependency::Input, t(0));
        let g1 = machine.generation();
        machine.fail_attempt(Dependency::Input, libc::EAGAIN, true, t(1));
        let g2 = machine.generation();
        machine.succeed(Dependency::Input, t(2));
        let g3 = machine.generation();
        assert!(g0 < g1 && g1 < g2 && g2 < g3);
        // A refused transition is not an edge.
        machine.succeed(Dependency::Input, t(3));
        assert_eq!(machine.generation(), g3);
    }

    #[test]
    fn the_report_is_a_single_line_with_fixed_key_order() {
        let mut machine = StartupMachine::new();
        machine.begin_attempt(Dependency::Input, t(0));
        machine.fail_attempt(Dependency::Input, libc::ETIMEDOUT, true, t(1));
        let report = machine.report();
        assert!(!report.contains('\n'));
        let order: Vec<usize> = [
            "assets=", "display=", "input=", "wayland=", "runtime=", "gpu=",
        ]
        .iter()
        .map(|key| report.find(key).expect(key))
        .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(report.contains(" input=backoff#1"));
    }
}
