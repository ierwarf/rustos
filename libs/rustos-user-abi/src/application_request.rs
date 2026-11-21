//! What an application says its Job needs: the *request* half of the
//! request/grant model for Application Job limits.
//!
//! - **Owner:** the package manifest declares a request; `runtime-control` and
//!   `runtimed` carry it unchanged; `loaderd` (Ring3 policy) turns it into a
//!   *grant* against the capacity it observes at launch
//!   (`services/loaderd/src/application_grant.rs`).
//! - **Boundary:** this module knows no machine capacity. A request is only
//!   checked for syntax (a declared value is non-zero) because every ceiling it
//!   could be compared with (`SYS_RUSTOS_TASK_CAPACITY`, physical memory, the
//!   threads other applications already hold) is a property of the running
//!   kernel, not of a package or a build. Stage-time validation therefore cannot
//!   and must not embed a capacity.
//! - **Wire:** `0` means "not requested" for that dimension; a declared value is
//!   never zero (`ApplicationRequest::new` rejects it at the source, the registry
//!   encodes absence as `0`). The all-zero request asks for the loader's
//!   derived defaults.
//! - **Semantics:** a declared dimension is the amount the application needs and
//!   is granted *exactly* or the launch is refused; a grant is never rounded
//!   up, clamped down or partially honoured.
//! - **Forbidden:** comparing a request with a literal ceiling, or treating a
//!   request as a grant.

use core::fmt;

/// Which dimension a diagnostic names; `manifest_key` is the manifest spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationRequestField {
    Processes,
    Threads,
    MemoryPagesPerProcess,
}

impl ApplicationRequestField {
    pub const fn manifest_key(self) -> &'static str {
        match self {
            Self::Processes => "request_processes",
            Self::Threads => "request_threads",
            Self::MemoryPagesPerProcess => "request_memory_pages_per_process",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationRequestError {
    /// A declared value was zero. Absence is the only way to ask for a default.
    Zero(ApplicationRequestField),
}

impl fmt::Display for ApplicationRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Zero(field) => write!(
                f,
                "{} must be non-zero (omit it to take the loader's derived default)",
                field.manifest_key()
            ),
        }
    }
}

impl core::error::Error for ApplicationRequestError {}

/// An application's request for its Job, as carried in the launch registry and
/// on the loader wire (`LoaderSpawnRequest::application_request`).
///
/// - `processes`: processes the Job may run at once.
/// - `threads`: threads the Job may run across all of its processes.
/// - `memory_pages_per_process`: resident/alias page budget of one process
///   (4 KiB pages); the Job total is this times the granted processes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct ApplicationRequest {
    pub processes: u64,
    pub threads: u64,
    pub memory_pages_per_process: u64,
}

impl ApplicationRequest {
    /// Nothing requested: every dimension takes the loader's derived default.
    pub const NONE: Self = Self { processes: 0, threads: 0, memory_pages_per_process: 0 };

    /// Whether any dimension was requested.
    pub const fn is_declared(&self) -> bool {
        self.processes != 0 || self.threads != 0 || self.memory_pages_per_process != 0
    }

    /// Source-side construction of an explicit declaration (`None` = absent).
    /// Syntax only: a present value must be non-zero.
    pub fn new(
        processes: Option<u64>,
        threads: Option<u64>,
        memory_pages_per_process: Option<u64>,
    ) -> Result<Self, ApplicationRequestError> {
        let present = |field, value: Option<u64>| match value {
            Some(0) => Err(ApplicationRequestError::Zero(field)),
            Some(value) => Ok(value),
            None => Ok(0),
        };
        Ok(Self {
            processes: present(ApplicationRequestField::Processes, processes)?,
            threads: present(ApplicationRequestField::Threads, threads)?,
            memory_pages_per_process: present(
                ApplicationRequestField::MemoryPagesPerProcess,
                memory_pages_per_process,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;

    #[test]
    fn nothing_requested_is_the_all_zero_wire() {
        assert_eq!(ApplicationRequest::default(), ApplicationRequest::NONE);
        assert!(!ApplicationRequest::NONE.is_declared());
        assert_eq!(core::mem::size_of::<ApplicationRequest>(), 24);
        assert_eq!(ApplicationRequest::new(None, None, None), Ok(ApplicationRequest::NONE));
    }

    #[test]
    fn a_declared_zero_is_a_syntax_error_not_a_default() {
        for (p, t, m, field) in [
            (Some(0), None, None, ApplicationRequestField::Processes),
            (None, Some(0), None, ApplicationRequestField::Threads),
            (None, None, Some(0), ApplicationRequestField::MemoryPagesPerProcess),
        ] {
            assert_eq!(
                ApplicationRequest::new(p, t, m),
                Err(ApplicationRequestError::Zero(field))
            );
        }
    }

    #[test]
    fn any_non_zero_value_is_accepted_because_no_capacity_is_known_here() {
        let huge = ApplicationRequest::new(Some(u64::MAX), Some(u64::MAX), Some(u64::MAX)).unwrap();
        assert!(huge.is_declared());
        let one = ApplicationRequest::new(None, Some(1), None).unwrap();
        assert_eq!(one, ApplicationRequest { processes: 0, threads: 1, memory_pages_per_process: 0 });
    }

    #[test]
    fn diagnostics_use_the_manifest_spelling() {
        let error = ApplicationRequestError::Zero(ApplicationRequestField::Threads);
        assert!(format!("{error}").starts_with("request_threads must be non-zero"));
    }
}
