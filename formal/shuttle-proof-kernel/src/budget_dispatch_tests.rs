/// PCT permutations of exact Context -> Domain preparation against separate
/// context release and domain generation replacement. Publication and reply
/// token races remain outside this bounded runtime-cell model.
#[test]
fn published_budget_prepare_never_touches_a_recycled_runtime_cell_pct() {
    shuttle::check_pct(
        || {
            let context = Arc::new(Mutex::new((1_usize, false)));
            let domain = Arc::new(Mutex::new((1_usize, false)));
            let context_writer = context.clone();
            let release = thread::spawn(move || {
                let mut cell = context_writer.lock().unwrap();
                *cell = (2, false);
            });
            let domain_writer = domain.clone();
            let replace = thread::spawn(move || {
                let mut cell = domain_writer.lock().unwrap();
                *cell = (2, false);
            });
            let context_reader = context.clone();
            let domain_reader = domain.clone();
            let prepare = thread::spawn(move || {
                let mut context = context_reader.lock().unwrap();
                let mut domain = domain_reader.lock().unwrap();
                if context.0 == 1 && domain.0 == 1 {
                    context.1 = true;
                    domain.1 = true;
                }
            });
            release.join().unwrap();
            replace.join().unwrap();
            prepare.join().unwrap();
            let context = context.lock().unwrap();
            let domain = domain.lock().unwrap();
            assert_eq!(context.0, 2);
            assert_eq!(domain.0, 2);
            assert!(!context.1 && !domain.1, "a reused cell inherited old dispatch preparation");
        },
        iterations(),
        pct_depth(),
    );
}

/// PCT sample of paired elapsed-credit and timeout-reply custody across two
/// independent runtime-cell generation replacements.
#[test]
fn published_charge_never_debits_a_recycled_pair_or_leaks_its_reply_pct() {
    shuttle::check_pct(
        || {
            let context = Arc::new(Mutex::new((1_usize, 10_u64, 0_u64)));
            let domain = Arc::new(Mutex::new((1_usize, 10_u64)));
            let context_writer = context.clone();
            let release = thread::spawn(move || {
                *context_writer.lock().unwrap() = (2, 10, 0);
            });
            let domain_writer = domain.clone();
            let replace = thread::spawn(move || {
                *domain_writer.lock().unwrap() = (2, 10);
            });
            let context_reader = context.clone();
            let domain_reader = domain.clone();
            let charge = thread::spawn(move || {
                let mut context = context_reader.lock().unwrap();
                let mut domain = domain_reader.lock().unwrap();
                if context.0 == 1 && domain.0 == 1 {
                    context.1 -= 10;
                    domain.1 -= 10;
                    context.2 = 77;
                }
            });
            release.join().unwrap();
            replace.join().unwrap();
            charge.join().unwrap();
            assert_eq!(*context.lock().unwrap(), (2, 10, 0));
            assert_eq!(*domain.lock().unwrap(), (2, 10));
        },
        iterations(),
        pct_depth(),
    );
}
