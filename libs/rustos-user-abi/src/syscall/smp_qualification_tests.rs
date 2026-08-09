use super::{
    RustosSmpQualificationBindArgs, SMP_QUALIFICATION_BIND_ABI_VERSION,
    SMP_QUALIFICATION_MAX_DEADLINE_MS, SMP_QUALIFICATION_MAX_WORK_UNITS,
    SMP_QUALIFICATION_MAX_WORKERS, pack_smp_qualification_worker,
    smp_qualification_bind_shape_valid, smp_qualification_worker_shape_valid,
    unpack_smp_qualification_worker,
};

pub(super) fn worker_shape_is_exact_and_bounded() {
    for cpu in 0..SMP_QUALIFICATION_MAX_WORKERS {
        for worker in 0..SMP_QUALIFICATION_MAX_WORKERS {
            assert_eq!(
                unpack_smp_qualification_worker(pack_smp_qualification_worker(cpu, worker)),
                (cpu, worker)
            );
        }
    }
    assert!(smp_qualification_worker_shape_valid(
        pack_smp_qualification_worker(7, 7),
        1,
        7,
    ));
    assert!(!smp_qualification_worker_shape_valid(
        pack_smp_qualification_worker(7, 7),
        1,
        6,
    ));
    assert!(!smp_qualification_worker_shape_valid(
        pack_smp_qualification_worker(0, SMP_QUALIFICATION_MAX_WORKERS),
        1,
        0,
    ));
    assert!(!smp_qualification_worker_shape_valid(
        pack_smp_qualification_worker(0, 0),
        0,
        0,
    ));
    assert!(!smp_qualification_worker_shape_valid(
        pack_smp_qualification_worker(0, 0),
        SMP_QUALIFICATION_MAX_WORK_UNITS + 1,
        0,
    ));
}

pub(super) fn bind_shape_is_closed_and_bounded() {
    let request = RustosSmpQualificationBindArgs {
        abi_version: SMP_QUALIFICATION_BIND_ABI_VERSION,
        target_pid: 41,
        workers: 8,
        work_units: SMP_QUALIFICATION_MAX_WORK_UNITS,
        deadline_ms: SMP_QUALIFICATION_MAX_DEADLINE_MS,
        ..RustosSmpQualificationBindArgs::default()
    };
    assert!(smp_qualification_bind_shape_valid(&request));

    let invalid_requests = [
        RustosSmpQualificationBindArgs {
            abi_version: SMP_QUALIFICATION_BIND_ABI_VERSION + 1,
            ..request
        },
        RustosSmpQualificationBindArgs {
            flags: 1,
            ..request
        },
        RustosSmpQualificationBindArgs {
            reserved0: 1,
            ..request
        },
        RustosSmpQualificationBindArgs {
            target_pid: 0,
            ..request
        },
        RustosSmpQualificationBindArgs {
            workers: 3,
            ..request
        },
        RustosSmpQualificationBindArgs {
            workers: 0,
            ..request
        },
        RustosSmpQualificationBindArgs {
            workers: SMP_QUALIFICATION_MAX_WORKERS + 1,
            ..request
        },
        RustosSmpQualificationBindArgs {
            reserved1: 1,
            ..request
        },
        RustosSmpQualificationBindArgs {
            work_units: 0,
            ..request
        },
        RustosSmpQualificationBindArgs {
            work_units: SMP_QUALIFICATION_MAX_WORK_UNITS + 1,
            ..request
        },
        RustosSmpQualificationBindArgs {
            deadline_ms: 0,
            ..request
        },
        RustosSmpQualificationBindArgs {
            deadline_ms: SMP_QUALIFICATION_MAX_DEADLINE_MS + 1,
            ..request
        },
        RustosSmpQualificationBindArgs {
            reserved2: 1,
            ..request
        },
    ];
    for invalid in invalid_requests {
        assert!(!smp_qualification_bind_shape_valid(&invalid));
    }
}
