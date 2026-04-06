use std::sync::{Mutex, MutexGuard};

static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn exclusive_test() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().expect("kernel test lock poisoned")
}
