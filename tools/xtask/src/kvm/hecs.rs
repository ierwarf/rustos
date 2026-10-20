//! Actual-execution supplement to the existing private SMP lifecycle proof.
//! FNV checks transport integrity, not authority: the exact prelaunch kernel,
//! private ELF and live binding admit these fixed-name frames. Never accept a
//! userspace PASS string, modeled native RSP, missing path or discarded frame.
use super::*;

const REGISTER_CHECKS: u64 = rustos_user_abi::syscall::SMP_QUALIFICATION_HECS_REGISTER_CHECKS;

#[derive(Debug, Serialize)]
pub(super) struct HecsWorkerEvidence {
    worker_id: u32,
    /// System-wide on this CPU during the private worker's window, not a
    /// private switch count or evidence that each spin received an interrupt.
    hardware_rsp_arrivals: VerifiedMilestoneFrame,
    register_checks: VerifiedMilestoneFrame,
}

#[derive(Debug, Serialize)]
pub(super) struct HecsEvidence {
    schema: &'static str,
    backend: &'static str,
    vcpus: u8,
    checks_per_path: u64,
    register_scope: &'static str,
    seed_scheme: &'static str,
    unverified_boundaries: &'static str,
    workers: Vec<HecsWorkerEvidence>,
}

pub(super) fn is_complete(log: &str, workers: u8) -> bool {
    evidence(log, workers).is_ok()
}

pub(super) fn evidence(log: &str, workers: u8) -> Result<HecsEvidence> {
    if runtime_stall_or_crash_observed(log) || log.lines().enumerate().any(|(line, text)| {
        parse_verified_milestone_frame(text, line + 1).is_some_and(|frame|
            frame.category == "compat" && frame.event == "linux-user-fault")
    }) {
        bail!("HECS observed a kernel panic, runtime failure or terminal user fault");
    }
    let lifecycle = verified_smp_qualification_events(log);
    validate_smp_ring3_qualification_events(&lifecycle, workers)?;
    let frames = log.lines().enumerate().filter_map(|(line, text)|
        parse_verified_milestone_frame(text, line + 1)).filter(|frame|
            frame.category == "compat" && matches!(frame.event.as_str(),
                "hecs-registers" | "hecs-rsp-arrivals")).collect::<Vec<_>>();
    if frames.len() != usize::from(workers) * 2 {
        bail!("HECS requires exactly one register and actual-RSP record per worker");
    }
    let mut ordered = lifecycle.iter().map(|frame|
        (frame.source_line, frame.output_seq, frame.milestone_seq)).chain(
        frames.iter().map(|frame| (frame.source_line, frame.output_seq, frame.milestone_seq)))
        .collect::<Vec<_>>();
    ordered.sort_by_key(|frame| frame.0);
    if !ordered.windows(2).all(|pair| pair[0].1 < pair[1].1)
        || ordered.iter().map(|frame| frame.2).collect::<BTreeSet<_>>().len() != ordered.len() {
        bail!("HECS replays or reorders a kernel evidence sequence");
    }
    let mut observations = Vec::new();
    for worker in 0..u32::from(workers) {
        let start = lifecycle.iter().find(|event|
            event.worker_id == worker && event.phase == "smp-qualification-start").unwrap();
        let finish = lifecycle.iter().find(|event|
            event.worker_id == worker && event.phase == "smp-qualification-finish").unwrap();
        let exact = |name: &str| -> Result<&VerifiedMilestoneFrame> {
            let matching = frames.iter().filter(|frame|
                frame.event == name && frame.event_arg0 as u32 == worker).collect::<Vec<_>>();
            if matching.len() != 1 { bail!("HECS missing or duplicate {name} for worker={worker}"); }
            let frame = matching[0];
            if (frame.event_arg0 >> 32) != u64::from(worker)
                || frame.event_arg1 >> SMP_QUALIFICATION_WORK_BITS != start.binding_id
                || frame.process_id != Some(start.process_id)
                || frame.thread_id != Some(start.thread_id)
                || frame.milestones_dropped != 0 || frame.debug_bytes_discarded != 0
                || frame.guest_ts_us < start.guest_ts_us || frame.guest_ts_us > finish.guest_ts_us {
                bail!("HECS foreign identity, CPU, binding, loss or observation window");
            }
            Ok(frame)
        };
        let rsp = exact("hecs-rsp-arrivals")?;
        let registers = exact("hecs-registers")?;
        if !(start.output_seq < rsp.output_seq && rsp.output_seq < registers.output_seq
            && registers.output_seq < finish.output_seq)
            || rsp.guest_ts_us > registers.guest_ts_us
            || rsp.event_arg1 & SMP_QUALIFICATION_WORK_MASK == 0
            || registers.event_arg1 & SMP_QUALIFICATION_WORK_MASK != REGISTER_CHECKS {
            bail!("HECS unexercised RSP arrival, partial register work or wrong phase order");
        }
        observations.push(HecsWorkerEvidence {
            worker_id: worker,
            hardware_rsp_arrivals: rsp.clone(),
            register_checks: registers.clone(),
        });
    }
    Ok(HecsEvidence {
        schema: "rustos-hecs-evidence-v1",
        backend: "KVM",
        vcpus: workers,
        checks_per_path: rustos_user_abi::syscall::SMP_QUALIFICATION_HECS_CHECKS_PER_PATH,
        register_scope: "assembly GPR/RSP/XMM0-15/MXCSR: gettid, yield, nanosleep, no-call spin; SYSCALL RAX/RCX/R11 excluded",
        seed_scheme: "0x123456789abcdef0 ^ (worker<<48) ^ (path<<32) ^ iteration",
        unverified_boundaries: "YMM/x87, IRQ specifically inside each spin, private switch/migration counts, TLB remapping, physical hardware",
        workers: observations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(workers: u8) -> Vec<VerifiedMilestoneFrame> {
        let mut frames = Vec::new();
        let mut push = |name: &str, worker: u8, count: u64| {
            let seq = frames.len() as u64 + 1;
            frames.push(VerifiedMilestoneFrame {
                source_line: seq as usize, output_seq: seq, milestone_seq: seq,
                guest_ts_us: seq * 100, guest_tick: seq,
                category: "compat".into(), event: name.into(),
                event_arg0: (u64::from(worker) << 32) | u64::from(worker),
                event_arg1: (1 << SMP_QUALIFICATION_WORK_BITS) | count,
                process_id: Some(41), thread_id: Some(100 + u64::from(worker)),
                milestones_dropped: 0, debug_bytes_discarded: 0, frame_checksum: 0,
            });
        };
        for worker in 0..workers { push("smp-qualification-ready", worker, SMP_QUALIFICATION_WORK_UNITS); }
        for worker in 0..workers { push("smp-qualification-start", worker, SMP_QUALIFICATION_WORK_UNITS); }
        for worker in 0..workers {
            push("hecs-rsp-arrivals", worker, 17);
            push("hecs-registers", worker, REGISTER_CHECKS);
            push("smp-qualification-finish", worker, SMP_QUALIFICATION_WORK_UNITS);
        }
        push("smp-qualification-complete", 0, SMP_QUALIFICATION_WORK_UNITS);
        frames
    }

    fn log(frames: &[VerifiedMilestoneFrame]) -> String {
        frames.iter().map(|f| {
            let semantic = format!("milestone-begin v=1 output_seq={} seq={} ts_us={} tick={} cat={} name={} arg0={:#x} arg1={:#x} pid={} tid={} dropped={} discarded_bytes={}",
                f.output_seq, f.milestone_seq, f.guest_ts_us, f.guest_tick, f.category,
                f.event, f.event_arg0, f.event_arg1, f.process_id.unwrap(), f.thread_id.unwrap(),
                f.milestones_dropped, f.debug_bytes_discarded);
            let checksum = milestone_frame_checksum(semantic.as_bytes());
            format!("seq={} ts_us={} tick={} lvl=info cat={} mod=nucleus_core::debug line=0 pid={} tid={} msg=\"{semantic} checksum={checksum:016x} milestone-end\"\n",
                f.output_seq, f.guest_ts_us, f.guest_tick, f.category, f.process_id.unwrap(), f.thread_id.unwrap())
        }).collect()
    }

    #[test]
    fn exact_hardware_register_and_rsp_evidence_is_required_for_every_topology() {
        for workers in [1, 2, 4, 8] {
            let records = fixture(workers);
            let accepted = evidence(&log(&records), workers).unwrap();
            assert!(is_complete(&log(&records), workers));
            assert_eq!(accepted.workers.len(), usize::from(workers));
            assert_eq!(accepted.checks_per_path, 256);
            let old = records.into_iter().filter(|f| !f.event.starts_with("hecs-")).collect::<Vec<_>>();
            assert!(smp_ring3_qualification_is_complete(&log(&old), workers));
            assert!(evidence(&log(&old), workers).is_err(), "SMP lifecycle alone is not HECS");
            assert!(!is_complete(&log(&old), workers));
        }
    }

    #[test]
    fn completed_hecs_rejects_kernel_panic_and_terminal_user_fault_on_any_cpu() {
        let valid = log(&fixture(8));
        assert!(evidence(&valid, 8).is_ok());
        for panic in ["[PANIC]", "!PANIC-SITE:00000203:kernel/hal/src/arch/tlb_shootdown.rs"] {
            assert!(evidence(&format!("{valid}\n{panic}\n"), 8).is_err());
            assert!(!is_complete(&format!("{valid}\n{panic}\n"), 8));
        }
        let mut fault = fixture(8).pop().unwrap();
        fault.output_seq += 1;
        fault.milestone_seq += 1;
        fault.event = "linux-user-fault".into();
        fault.process_id = Some(21); // A boot service, not a private worker.
        fault.thread_id = Some(21);
        let damaged = format!("{valid}\n{}", log(&[fault]));
        assert!(evidence(&damaged, 8).is_err());
        assert!(!is_complete(&damaged, 8));
    }

    #[test]
    fn missing_duplicate_foreign_lost_zero_partial_and_replayed_hecs_are_rejected() {
        let pristine = fixture(2);
        let rsp = pristine.iter().position(|f| f.event == "hecs-rsp-arrivals").unwrap();
        let regs = rsp + 1;
        for fault in 0..15 {
            let mut frames = pristine.clone();
            match fault {
                0 => { frames.remove(rsp); }
                1 => frames.push(frames[regs].clone()),
                2 => frames[rsp].event_arg1 = 1 << SMP_QUALIFICATION_WORK_BITS,
                3 => frames[regs].event_arg1 -= 1,
                4 => frames[regs].event_arg0 ^= 1 << 32,
                5 => frames[regs].process_id = Some(42),
                6 => frames[rsp].thread_id = Some(101),
                7 => frames[regs].event_arg1 += 1 << SMP_QUALIFICATION_WORK_BITS,
                8 => frames[regs].milestones_dropped = 1,
                9 => frames[rsp].debug_bytes_discarded = 1,
                10 => frames[regs].milestone_seq = frames[0].milestone_seq,
                11 => frames[rsp].guest_ts_us = 0,
                12 => frames.swap(rsp, regs),
                13 => frames[regs].event = "user-debug payload=hecs-registers".into(),
                14 => frames[regs].event_arg0 = (1 << 32) | 1,
                _ => unreachable!(),
            }
            assert!(evidence(&log(&frames), 2).is_err(), "fault={fault}");
            assert!(!is_complete(&log(&frames), 2), "actual readiness helper fault={fault}");
        }
        let valid = log(&pristine);
        assert!(evidence(&valid.replace("name=hecs-registers", "name=hecs-registerz"), 2).is_err());
        assert!(evidence("user-debug payload=hecs-registers PASS", 1).is_err());
    }
}
