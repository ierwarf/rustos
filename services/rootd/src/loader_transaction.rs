//! A loader transaction must keep rootd's authorization receiver available.
//! Start failure executes no blocking call. Every completed call, including a
//! refusal, joins the supervisor before its borrowed lease storage is reused.
pub(super) fn run(
    start: impl FnOnce() -> Result<(), i64>,
    operation: impl FnOnce() -> Result<u64, i64>,
    complete: impl FnOnce(Result<u64, i64>) -> Result<u64, i64>,
) -> Result<u64, i64> {
    start()?;
    let result = operation();
    complete(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::{Cell, RefCell}, sync::mpsc, thread, time::Duration};

    #[test]
    fn earlier_spawn_vfs_authorization_can_complete_before_realm_configuration() {
        // Regression schedule: the serial loader first owes a pending spawn
        // its VFSD lookup; only then can it receive rootd's configuration.
        let (lookup, requests) = mpsc::channel();
        let (approve, approved) = mpsc::channel();
        let worker = RefCell::new(None);
        let result = run(
            || {
                *worker.borrow_mut() = Some(thread::spawn(move || {
                    assert_eq!(requests.recv_timeout(Duration::from_secs(1)).unwrap(), "vfsd");
                    approve.send(()).unwrap();
                }));
                Ok(())
            },
            || {
                lookup.send("vfsd").map_err(|_| 5_i64)?;
                approved.recv_timeout(Duration::from_secs(1)).map_err(|_| 110_i64)?;
                Ok(1) // only now may the loader accept configuration
            },
            |result| {
                worker.borrow_mut().take().unwrap().join().unwrap();
                result
            },
        );
        assert_eq!(result, Ok(1));
    }

    #[test]
    fn failed_start_never_blocks_and_failed_transaction_still_joins() {
        let called = Cell::new(false);
        let joined = Cell::new(false);
        assert_eq!(run(|| Err(11), || { called.set(true); Ok(1) }, |result| {
            joined.set(true); result
        }), Err(11));
        assert!(!called.get() && !joined.get());
        assert_eq!(run(|| Ok(()), || { called.set(true); Err(13) }, |result| {
            joined.set(true); result
        }), Err(13));
        assert!(called.get() && joined.get());
    }
}
