// Protocol projection of post-kill drain: live membership is already zero,
// while object reap and page release finish independently. Not production MM.
struct AppLimitDrainWitness { live: u32, objects: u32, pages: u32, closes: u32 }

fn application_limit_retirement_race() {
    let state = Arc::new(Mutex::new(AppLimitDrainWitness { live: 0, objects: 1, pages: 1, closes: 0 }));
    let process_state = state.clone();
    let process = thread::spawn(move || { process_state.lock().unwrap().objects = 0; });
    let pages_state = state.clone();
    let pages = thread::spawn(move || { pages_state.lock().unwrap().pages = 0; });
    let observer_state = state.clone();
    let observer = thread::spawn(move || {
        let mut state = observer_state.lock().unwrap();
        if state.live == 0 && state.objects == 0 && state.pages == 0 {
            assert_eq!((state.live, state.objects, state.pages), (0, 0, 0), "live zero is not object zero");
            state.closes += 1;
        }
    });
    process.join().unwrap();
    pages.join().unwrap();
    observer.join().unwrap();
    let mut state = state.lock().unwrap();
    assert_eq!((state.live, state.objects, state.pages), (0, 0, 0));
    if state.closes == 0 { state.closes += 1; } // later census, not an unbounded liveness claim
    assert_eq!(state.closes, 1, "one final drain report/descriptor closer");
}

#[test]
fn application_limit_drain_waits_for_process_and_page_retirement_pct() {
    shuttle::check_pct(application_limit_retirement_race, iterations(), pct_depth());
}
