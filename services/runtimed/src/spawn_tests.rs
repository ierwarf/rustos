#[test]
    fn endpoint_lookup_preserves_real_libc_errno() {
        // This RustOS syscall is deliberately absent on the native Linux host.
        // Exercise libc's actual -1/errno boundary through the production call.
        assert_eq!(
            super::lookup_service_endpoint(super::IPC_SERVICE_LOADERD),
            -i64::from(libc::ENOSYS)
        );
    }

    #[test]
    fn syscall_status_preserves_timeout_and_nonerror_results() {
        // SAFETY: an invalid Linux syscall has no host effects or pointers.
        let status = unsafe { libc::syscall(-1 as libc::c_long) as i64 };
        assert_eq!(super::libc_syscall_status(status), -i64::from(libc::ENOSYS));
        assert_eq!(super::libc_syscall_status(-i64::from(libc::ETIMEDOUT)), -i64::from(libc::ETIMEDOUT));
        assert_eq!(super::libc_syscall_status(0), 0);
        assert_eq!(super::libc_syscall_status(73), 73);
    }

    #[test]
    fn an_empty_running_set_never_enters_the_wait4_policy_round_trip() {
        let source = include_str!("spawn.rs");
        let reap = source
            .split_once("pub(super) fn reap_children(")
            .expect("reap entry")
            .1
            .split_once("/// How long the broker may stay idle")
            .expect("reap boundary")
            .0;
        let empty_guard = reap
            .find("if state.running.is_empty() {")
            .expect("empty owner-set guard");
        let wait4 = reap.find("libc::SYS_wait4").expect("wait4 policy call");
        assert!(empty_guard < wait4, "empty guard must precede wait4");
    }

    /// The broker loop must not perform the launch's storage reads.
    ///
    /// `begin_tracked_launch` runs on the loop that is the console's only
    /// receiver; the worker exists so the blocking calls do not. The acceptance
    /// contract is the easiest one to put back by accident, because it looks
    /// like a cheap entry rewrite and is in fact a DVM-volume read that
    /// deliberately does not cache its failures - so before storaged is up it
    /// repeats once per launch attempt, and it measured 83 ms per attempt when
    /// it ran here.
    #[test]
    fn the_loop_side_of_a_launch_performs_no_storage_read() {
        let source = include_str!("spawn.rs");
        let body = source
            .split_once("pub(super) fn begin_tracked_launch(")
            .expect("the loop-side entry point exists")
            .1;
        let body = body
            .split_once("\n/// Apply whatever the launch worker has reported")
            .expect("it ends before the completion handler")
            .0;
        assert!(
            !body.contains("apply_kvm_acceptance_contract("),
            "the acceptance contract is a storage read and belongs to the worker"
        );
        assert!(
            !body.contains("load_kvm_acceptance_contract("),
            "nor may it be read directly here"
        );
        assert!(
            !body.contains("spawn_exec("),
            "the loader round trip belongs to the worker"
        );
    }

    use std::cell::RefCell;

    use super::{
        admitted_task_weight_micros, bind_then_activate_spawned_process, build_exec_argv,
        build_exec_env_with_defaults, cleanup_failed_spawn, parse_kvm_acceptance_contract,
        smp_qualification_bind_args, upsert_env,
    };
    use crate::{
        kvm_smp_qualification::KvmSmpQualificationContract, MAX_UNTRUSTED_TASK_WEIGHT_MICROS,
        UI_SERVER_EXEC_PATH, UI_SERVER_TASK_WEIGHT_MICROS,
    };
    use rustos_user_abi::syscall::SMP_QUALIFICATION_BIND_ABI_VERSION;

    const QUALIFICATION: KvmSmpQualificationContract = KvmSmpQualificationContract {
        workers: 4,
        work_units: 1_000_000,
        deadline_ms: 5_000,
    };

    #[test]
    fn qualification_bind_is_exact_and_precedes_activation() {
        let args = smp_qualification_bind_args(41, QUALIFICATION).expect("valid bind wire");
        assert_eq!(args.abi_version, SMP_QUALIFICATION_BIND_ABI_VERSION);
        assert_eq!(args.target_pid, 41);
        assert_eq!(args.workers, 4);
        assert_eq!(args.work_units, 1_000_000);
        assert_eq!(args.deadline_ms, 5_000);
        assert_eq!(args.flags, 0);
        assert_eq!(args.reserved0, 0);
        assert_eq!(args.reserved1, 0);
        assert_eq!(args.reserved2, 0);
        assert!(smp_qualification_bind_args(0, QUALIFICATION).is_err());
        assert!(smp_qualification_bind_args(-1, QUALIFICATION).is_err());
        assert!(smp_qualification_bind_args(
            41,
            KvmSmpQualificationContract {
                deadline_ms: 5_001,
                ..QUALIFICATION
            }
        )
        .is_err());

        let calls = RefCell::new(Vec::new());
        bind_then_activate_spawned_process(
            41,
            Some(QUALIFICATION),
            |pid, contract| {
                assert_eq!((pid, contract), (41, QUALIFICATION));
                calls.borrow_mut().push("bind");
                Ok(())
            },
            |pid| {
                assert_eq!(pid, 41);
                calls.borrow_mut().push("activate");
                Ok(())
            },
        )
        .expect("bind then activate");
        assert_eq!(&*calls.borrow(), &["bind", "activate"]);
    }

    #[test]
    fn qualification_bind_failure_never_activates_the_child() {
        let activation_count = RefCell::new(0_u32);
        let failure = bind_then_activate_spawned_process(
            42,
            Some(QUALIFICATION),
            |_, _| Err(libc::EPERM),
            |_| {
                *activation_count.borrow_mut() += 1;
                Ok(())
            },
        )
        .expect_err("bind failure must be terminal");
        assert_eq!(failure.1, libc::EPERM);
        assert_eq!(*activation_count.borrow(), 0);
    }

    #[test]
    fn ordinary_launch_skips_private_bind_and_activates_once() {
        let bind_count = RefCell::new(0_u32);
        let activation_count = RefCell::new(0_u32);
        bind_then_activate_spawned_process(
            43,
            None,
            |_, _| {
                *bind_count.borrow_mut() += 1;
                Ok(())
            },
            |pid| {
                assert_eq!(pid, 43);
                *activation_count.borrow_mut() += 1;
                Ok(())
            },
        )
        .expect("ordinary activation");
        assert_eq!(*bind_count.borrow(), 0);
        assert_eq!(*activation_count.borrow(), 1);
    }

    #[test]
    fn catalog_weight_cannot_promote_an_untrusted_program() {
        assert_eq!(
            admitted_task_weight_micros("apps/shell/shell.elf", u64::MAX),
            MAX_UNTRUSTED_TASK_WEIGHT_MICROS
        );
    }

    #[test]
    fn private_acceptance_contract_is_exact_and_boolean() {
        let contract = parse_kvm_acceptance_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=1\nnetwork_exercise=0\n",
        )
        .expect("valid private acceptance contract");
        assert!(contract.ui_profile);
        assert!(!contract.network_exercise);
        assert!(parse_kvm_acceptance_contract("ui_profile=1\nnetwork_exercise=0\n").is_none());
        assert!(parse_kvm_acceptance_contract(
            "contract=rustos-kvm-acceptance-v1\nui_profile=yes\nnetwork_exercise=0\n"
        )
        .is_none());
    }

    #[test]
    fn acceptance_environment_replaces_manifest_default_once() {
        let mut env = vec![String::from("RUSTOS_UI_PROFILE=0"), String::from("A=1")];
        upsert_env(&mut env, "RUSTOS_UI_PROFILE=1");
        assert_eq!(
            env.iter()
                .filter(|value| value.starts_with("RUSTOS_UI_PROFILE="))
                .count(),
            1
        );
        assert!(env.iter().any(|value| value == "RUSTOS_UI_PROFILE=1"));
    }

    #[test]
    fn only_the_exact_ui_server_path_receives_system_weight() {
        assert_eq!(
            admitted_task_weight_micros(UI_SERVER_EXEC_PATH, 1),
            UI_SERVER_TASK_WEIGHT_MICROS
        );
        assert_eq!(
            admitted_task_weight_micros("services/uiserver/uiserver.elf.bak", 2_000),
            MAX_UNTRUSTED_TASK_WEIGHT_MICROS
        );
    }

    #[test]
    fn build_exec_argv_defaults_to_exec_path() {
        let argv = build_exec_argv("apps/demo/demo.elf", &[]).expect("valid default argv");
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0].to_str().unwrap(), "apps/demo/demo.elf");
    }

    #[test]
    fn build_exec_env_preserves_explicit_values_and_adds_defaults() {
        let defaults = [
            String::from("PATH=/bin:/usr/bin:/usr/local/bin"),
            String::from("HOME=/home/user"),
            String::from("XDG_RUNTIME_DIR=/run/user/1000"),
            String::from("WAYLAND_DISPLAY=wayland-0"),
            String::from("XDG_SESSION_TYPE=wayland"),
            String::from("XDG_CURRENT_DESKTOP=RustOS"),
        ];
        let env = build_exec_env_with_defaults(
            &[
                String::from("PATH=/custom/bin"),
                String::from("XDG_RUNTIME_DIR=/run/custom"),
            ],
            &defaults,
        )
        .expect("valid merged environment");
        let values = env
            .iter()
            .map(|item| item.to_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(values.iter().any(|item| item == "PATH=/custom/bin"));
        assert!(values
            .iter()
            .any(|item| item == "XDG_RUNTIME_DIR=/run/custom"));
        assert!(values.iter().any(|item| item == "HOME=/home/user"));
        assert!(values
            .iter()
            .any(|item| item == "WAYLAND_DISPLAY=wayland-0"));
        assert!(values.iter().any(|item| item == "XDG_SESSION_TYPE=wayland"));
        assert!(values
            .iter()
            .any(|item| item == "XDG_CURRENT_DESKTOP=RustOS"));
        assert!(!values
            .iter()
            .any(|item| item == "PATH=/bin:/usr/bin:/usr/local/bin"));
        assert!(!values
            .iter()
            .any(|item| item == "XDG_RUNTIME_DIR=/run/user/1000"));
    }

    #[test]
    fn build_exec_argv_rejects_invalid_input_instead_of_substituting_a_path() {
        assert_eq!(
            build_exec_argv("apps/demo\0demo.elf", &[]),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn failed_spawn_cleanup_accepts_only_exact_retirement_or_esrch() {
        let mut retired = 0;
        assert_eq!(
            cleanup_failed_spawn(77, |pid| {
                retired = pid;
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(retired, 77);
        assert_eq!(cleanup_failed_spawn(78, |_| Err(libc::ESRCH)), Ok(()));
        assert_eq!(
            cleanup_failed_spawn(79, |_| Err(libc::EPERM)),
            Err(libc::EPERM)
        );
        assert_eq!(cleanup_failed_spawn(0, |_| Ok(())), Err(libc::EINVAL));
    }
