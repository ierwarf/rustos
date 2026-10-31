//! Hierarchical accounting units and checked admission arithmetic.
//!
//! No CPU-local stock or speculative overcharge exists in this *policy* model.
//! Real kernel frame charging remains with its current atomic owner; callers
//! use the snapshot to validate service/session decisions, never to substitute
//! for the authoritative kernel job ledger.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Charge {
    pub processes: u64,
    pub threads: u64,
    pub user_pages: u64,
    pub kernel_bytes: u64,
    pub handles: u64,
    pub sessions: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChargeError {
    Overflow,
    Underflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaError {
    Exceeded,
    InvalidLimit,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    /// `u64::MAX` means unbounded by this local policy; an ancestor can
    /// still impose a tighter limit.
    pub maximum: Charge,
}

impl Quota {
    pub const UNLIMITED: Self = Self {
        maximum: Charge {
            processes: u64::MAX,
            threads: u64::MAX,
            user_pages: u64::MAX,
            kernel_bytes: u64::MAX,
            handles: u64::MAX,
            sessions: u64::MAX,
        },
    };

    pub fn validate_tightening(self, next: Self) -> Result<Self, QuotaError> {
        if next.maximum.fits_in(self.maximum) {
            Ok(next)
        } else {
            Err(QuotaError::InvalidLimit)
        }
    }
}

impl Charge {
    pub const ZERO: Self = Self {
        processes: 0,
        threads: 0,
        user_pages: 0,
        kernel_bytes: 0,
        handles: 0,
        sessions: 0,
    };

    pub const fn fits_in(self, limit: Self) -> bool {
        self.processes <= limit.processes
            && self.threads <= limit.threads
            && self.user_pages <= limit.user_pages
            && self.kernel_bytes <= limit.kernel_bytes
            && self.handles <= limit.handles
            && self.sessions <= limit.sessions
    }

    pub fn checked_add(self, other: Self) -> Result<Self, ChargeError> {
        Ok(Self {
            processes: self
                .processes
                .checked_add(other.processes)
                .ok_or(ChargeError::Overflow)?,
            threads: self
                .threads
                .checked_add(other.threads)
                .ok_or(ChargeError::Overflow)?,
            user_pages: self
                .user_pages
                .checked_add(other.user_pages)
                .ok_or(ChargeError::Overflow)?,
            kernel_bytes: self
                .kernel_bytes
                .checked_add(other.kernel_bytes)
                .ok_or(ChargeError::Overflow)?,
            handles: self
                .handles
                .checked_add(other.handles)
                .ok_or(ChargeError::Overflow)?,
            sessions: self
                .sessions
                .checked_add(other.sessions)
                .ok_or(ChargeError::Overflow)?,
        })
    }

    pub fn checked_sub(self, other: Self) -> Result<Self, ChargeError> {
        Ok(Self {
            processes: self
                .processes
                .checked_sub(other.processes)
                .ok_or(ChargeError::Underflow)?,
            threads: self
                .threads
                .checked_sub(other.threads)
                .ok_or(ChargeError::Underflow)?,
            user_pages: self
                .user_pages
                .checked_sub(other.user_pages)
                .ok_or(ChargeError::Underflow)?,
            kernel_bytes: self
                .kernel_bytes
                .checked_sub(other.kernel_bytes)
                .ok_or(ChargeError::Underflow)?,
            handles: self
                .handles
                .checked_sub(other.handles)
                .ok_or(ChargeError::Underflow)?,
            sessions: self
                .sessions
                .checked_sub(other.sessions)
                .ok_or(ChargeError::Underflow)?,
        })
    }

    pub fn admit(self, cost: Self, quota: Quota) -> Result<Self, QuotaError> {
        let next = self.checked_add(cost).map_err(|_| QuotaError::Overflow)?;
        if next.fits_in(quota.maximum) {
            Ok(next)
        } else {
            Err(QuotaError::Exceeded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_arithmetic_never_partial_adds() {
        let used = Charge {
            threads: u64::MAX,
            user_pages: 10,
            ..Charge::ZERO
        };
        let cost = Charge {
            threads: 1,
            user_pages: 1,
            ..Charge::ZERO
        };
        assert_eq!(used.checked_add(cost), Err(ChargeError::Overflow));
        assert_eq!(used.user_pages, 10);
        assert_eq!(Charge::ZERO.checked_sub(cost), Err(ChargeError::Underflow));
    }

    #[test]
    fn limits_cannot_be_widened_by_child() {
        let root = Quota {
            maximum: Charge {
                processes: 2,
                ..Quota::UNLIMITED.maximum
            },
        };
        assert_eq!(
            root.validate_tightening(Quota::UNLIMITED),
            Err(QuotaError::InvalidLimit)
        );
        assert!(root.validate_tightening(root).is_ok());
    }
}
