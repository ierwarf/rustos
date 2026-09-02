use core::sync::atomic::{AtomicU64, Ordering};

use super::task_id::allocate_task_id_from;

#[test]
fn task_identity_exhaustion_never_wraps_to_a_live_id() {
    let counter = AtomicU64::new(u64::MAX - 1);
    assert_eq!(allocate_task_id_from(&counter), Some(u64::MAX - 1));
    assert_eq!(allocate_task_id_from(&counter), None);
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}
