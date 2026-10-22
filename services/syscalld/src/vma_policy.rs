pub trait AddressRange {
    fn start(&self) -> u64;
    fn end(&self) -> u64;
}

/// munmap as one transaction: the kernel revocation of the whole interval runs
/// first, and policy bookkeeping is removed only after it succeeded. A hole in
/// the policy table is still a valid unmap (legacy leaves may sit there), so
/// the revocation always runs.
pub fn unmap_range_with<S, E>(
    state: &mut S,
    start: u64,
    end: u64,
    revoke: impl FnOnce(u64, u64) -> Result<(), E>,
    remove_policy: impl FnOnce(&mut S, u64, u64),
) -> Result<(), E> {
    revoke(start, end)?;
    remove_policy(state, start, end);
    Ok(())
}

pub fn next_fit_with_wrap<T: AddressRange>(
    ranges: &[T],
    lower_bound: u64,
    upper_bound: u64,
    hint: u64,
    len: u64,
    alignment: u64,
) -> Option<u64> {
    let lower_bound = align_up(lower_bound, alignment)?;
    let start = align_up(hint, alignment)?.max(lower_bound);
    if start >= upper_bound {
        return find_between(ranges, lower_bound, upper_bound, len, alignment);
    }
    find_between(ranges, start, upper_bound, len, alignment).or_else(|| {
        (start > lower_bound)
            .then(|| find_between(ranges, lower_bound, start, len, alignment))
            .flatten()
    })
}

fn find_between<T: AddressRange>(
    ranges: &[T],
    mut cursor: u64,
    limit: u64,
    len: u64,
    alignment: u64,
) -> Option<u64> {
    loop {
        let end = cursor.checked_add(len)?;
        if end > limit {
            return None;
        }
        if let Some(conflict) = ranges
            .iter()
            .find(|range| cursor < range.end() && end > range.start())
        {
            cursor = align_up(conflict.end(), alignment)?;
            continue;
        }
        return Some(cursor);
    }
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn sparse_munmap_is_one_transaction_and_commits_policy_only_on_success() {
        use super::unmap_range_with;
        // (start, end) policy intervals; an empty one is (0, 0).
        let mut policy = [(0x4000_u64, 0x5000_u64), (0x6000, 0x7000)];
        let remove = |p: &mut [(u64, u64); 2], start: u64, end: u64| {
            for range in p.iter_mut() {
                if start < range.1 && range.0 < end {
                    *range = (0, 0);
                }
            }
        };
        let mut calls = 0;
        let refused = unmap_range_with(
            &mut policy,
            0x4000,
            0x7000,
            |start, end| {
                calls += 1;
                assert_eq!((start, end), (0x4000, 0x7000));
                Err(12)
            },
            remove,
        );
        assert_eq!(refused, Err(12));
        assert_eq!(calls, 1, "the whole interval is one revocation");
        assert_eq!(policy, [(0x4000, 0x5000), (0x6000, 0x7000)], "a refusal keeps policy");
        assert_eq!(
            unmap_range_with(&mut policy, 0x4000, 0x7000, |_, _| Ok::<(), i32>(()), remove),
            Ok(())
        );
        assert_eq!(policy, [(0, 0), (0, 0)]);
        // A repeated unmap of an existing hole still reaches the revocation.
        let mut reached = false;
        assert_eq!(
            unmap_range_with(
                &mut policy,
                0x4000,
                0x7000,
                |_, _| {
                    reached = true;
                    Ok::<(), i32>(())
                },
                remove
            ),
            Ok(())
        );
        assert!(reached);
    }

    use super::*;

    #[derive(Clone, Copy)]
    struct Range(u64, u64);

    impl AddressRange for Range {
        fn start(&self) -> u64 {
            self.0
        }

        fn end(&self) -> u64 {
            self.1
        }
    }

    #[test]
    fn next_fit_wraps_cursor_and_reuses_a_freed_gap() {
        let ranges = [Range(0x2000, 0x4000), Range(0x8000, 0x10_000)];
        assert_eq!(
            next_fit_with_wrap(&ranges, 0x2000, 0x10_000, 0xf000, 0x3000, 0x1000),
            Some(0x4000)
        );
    }

    #[test]
    fn next_fit_never_crosses_live_ranges_or_wrap_limit() {
        let ranges = [Range(0x4000, 0x8000)];
        assert_eq!(
            next_fit_with_wrap(&ranges, 0x2000, 0xa000, 0x3000, 0x3000, 0x1000),
            None
        );
    }

    #[test]
    fn out_of_range_hint_wraps_only_inside_the_user_window() {
        let ranges = [Range(0x2000, 0x4000)];
        assert_eq!(
            next_fit_with_wrap(&ranges, 0x2000, 0x10_000, 0x20_000, 0x2000, 0x1000),
            Some(0x4000)
        );
    }
}
