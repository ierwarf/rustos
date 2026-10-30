/// Bounded projection of the process-table leaf: one non-root job slot, one
/// member object and one exact spawn transaction. The root is permanent.
/// These protocol witnesses do not execute the production spinlock or model
/// address-space teardown; native tests separately exercise production guards.
#[derive(Clone, Copy)]
struct JobSlotWitness {
    generation: u32,
    in_use: bool,
    killing: bool,
    member: Option<(u32, bool)>, // job generation, exiting (still an object)
    reserved: bool,
}

impl JobSlotWitness {
    fn live() -> Self {
        Self { generation: 1, in_use: true, killing: false, member: None, reserved: false }
    }

    fn check(&self) {
        if let Some((generation, _)) = self.member {
            assert!(self.in_use, "an unreaped object pins its job slot");
            assert_eq!(generation, self.generation, "a member cannot alias a new job");
        }
    }

    fn release(&mut self) {
        // release_dead_jobs uses ANY object, including an exiting member.
        if self.in_use && self.killing && self.member.is_none() {
            self.in_use = false;
            self.generation += 1;
        }
        self.check();
    }

    fn recreate(&mut self) {
        // A separate create operation may reuse the now-empty job slot.
        if !self.in_use {
            self.in_use = true;
            self.killing = false;
        }
        self.check();
    }
}

fn job_reservation_race() {
    let table = Arc::new(Mutex::new(JobSlotWitness::live()));
    let publisher_table = table.clone();
    let publisher = thread::spawn(move || {
        // reserve_spawn_in checks the explicit job before taking the table
        // leaf to reserve a process slot. Neither step pins the job itself.
        let generation = 1; // the caller's explicit JobId, never refreshed
        {
            let table = publisher_table.lock().unwrap();
            if !table.in_use || table.generation != generation || table.killing { return; }
        }
        publisher_table.lock().unwrap().reserved = true;
        let mut table = publisher_table.lock().unwrap();
        // publish_inner resolves the FULL job id and installs the object in
        // the same process-table transaction. Refusal retains rollback custody.
        if table.in_use && table.generation == generation && !table.killing {
            assert!(table.reserved);
            assert!(!table.killing, "no member joins after the kill linearization");
            assert_eq!(table.generation, generation);
            table.member = Some((generation, false));
            table.reserved = false;
        } else {
            assert!(table.reserved);
            table.reserved = false; // exact cancel_spawn
        }
        table.check();
    });
    let killer_table = table.clone();
    let killer = thread::spawn(move || {
        {
            let mut table = killer_table.lock().unwrap();
            table.killing = true;
            table.release();
        }
        killer_table.lock().unwrap().recreate();
    });
    publisher.join().unwrap();
    killer.join().unwrap();
    let table = table.lock().unwrap();
    table.check();
    assert!(!table.reserved);
    if let Some((generation, _)) = table.member {
        assert_eq!(generation, 1, "a stale reservation cannot join the replacement");
        assert!(table.killing, "a pre-kill publication must retain its killed job");
    }
}

fn job_reap_race() {
    let table = Arc::new(Mutex::new(JobSlotWitness {
        killing: true, member: Some((1, true)), ..JobSlotWitness::live()
    }));
    let reaper_table = table.clone();
    let reaper = thread::spawn(move || {
        let mut table = reaper_table.lock().unwrap();
        table.check();
        assert_eq!(table.member, Some((1, true)));
        table.member = None; // reap removes the object, not merely its live flag
    });
    let collector_table = table.clone();
    let collector = thread::spawn(move || {
        collector_table.lock().unwrap().release();
        collector_table.lock().unwrap().recreate();
    });
    reaper.join().unwrap();
    collector.join().unwrap();
    let mut table = table.lock().unwrap();
    // Reap does not itself release jobs. A later collection completes reuse
    // even when the first collector ran before the reaper.
    table.release();
    table.recreate();
    assert_eq!(table.generation, 2);
    assert!(table.in_use && !table.killing);
    assert!(table.member.is_none());
    assert_ne!(table.generation, 1, "old inspect/kill/publication ids are stale");
}

#[test]
fn job_spawn_reservation_cannot_publish_into_killed_or_reused_job() {
    loom::model(job_reservation_race);
}

#[test]
fn job_reuse_waits_for_last_member_object_reap() {
    loom::model(job_reap_race);
}
