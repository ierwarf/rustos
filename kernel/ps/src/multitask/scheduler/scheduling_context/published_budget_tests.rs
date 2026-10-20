    #[test]
    fn published_budget_stamp_rejects_a_cpu_outside_its_live_policy_mask() {
        let _runtime = super::super::scheduling_runtime::test_serial_guard();
        super::super::scheduling_runtime::reset_for_test();
        let context_slot = 58;
        let domain_slot = 59;
        let policy = policy(2);
        let domain_generation =
            super::super::scheduling_runtime::initialize_domain(domain_slot, policy)
                .expect("domain admission");
        let mut context = SchedulingContext::bind(context_slot, 0xabc);
        assert!(context.admit(policy, domain_slot, domain_generation));
        let identity = context.identity();
        let stamp = super::super::super::current_identity::PagerChargeStamp {
            context_slot: identity.slot(),
            context_generation: identity.generation(),
            scheduling_domain: policy.domain,
            domain_generation: u64::from(domain_generation),
            policy_epoch: policy.policy_epoch,
            period_ns: policy.period_ns,
        };
        assert_eq!(
            super::super::scheduling_runtime::published_stamp_admission(stamp, 0, 0),
            Some(true),
        );
        assert_eq!(
            super::super::scheduling_runtime::published_stamp_admission(stamp, 1, 0),
            None,
        );
        assert_eq!(
            super::super::scheduling_runtime::published_stamp_admission(
                super::super::super::current_identity::PagerChargeStamp {
                    context_generation: stamp.context_generation.wrapping_add(1),
                    ..stamp
                },
                0,
                0,
            ),
            None,
        );
        assert!(super::super::scheduling_runtime::published_stamp_is_eligible(stamp, 0, 0));
        assert!(super::super::scheduling_runtime::prepare_published_stamp_dispatch(stamp, 0, 0));
        assert!(!super::super::scheduling_runtime::prepare_published_stamp_dispatch(stamp, 1, 0));
        assert!(!super::super::scheduling_runtime::prepare_published_stamp_dispatch(
            super::super::super::current_identity::PagerChargeStamp {
                context_generation: stamp.context_generation.wrapping_add(1),
                ..stamp
            },
            0,
            0,
        ));
        assert!(!super::super::scheduling_runtime::prepare_published_stamp_dispatch(
            super::super::super::current_identity::PagerChargeStamp {
                domain_generation: stamp.domain_generation.wrapping_add(1),
                ..stamp
            },
            0,
            0,
        ));
        assert_eq!(context.runtime_snapshot().unwrap().available_ns, policy.budget_ns);
        assert!(super::super::scheduling_runtime::charge_domain_runtime(
            domain_slot,
            domain_generation,
            policy,
            0,
            1,
            policy.budget_ns,
        )
        .is_some());
        assert_eq!(
            super::super::scheduling_runtime::published_stamp_admission(stamp, 0, 1),
            Some(false),
        );
        assert_eq!(
            super::super::scheduling_runtime::prepare_published_stamp_dispatch_status(stamp, 0, 1),
            super::super::scheduling_runtime::PublishedDispatchPreparation::DomainRefused,
        );
        assert_eq!(
            super::super::scheduling_runtime::published_domain_refusal_cause(stamp, 0, 1),
            Some((domain_slot, DomainRefusalCause::EmptyRefillPending)),
        );
        assert_eq!(
            super::super::scheduling_runtime::published_domain_refusal_cause(
                super::super::super::current_identity::PagerChargeStamp {
                    context_generation: stamp.context_generation.wrapping_add(1),
                    ..stamp
                },
                0,
                1,
            ),
            None,
        );
        assert_eq!(
            super::super::scheduling_runtime::published_stamp_admission(stamp, 0, policy.period_ns + 1),
            Some(true),
        );
        assert_eq!(context.runtime_snapshot().unwrap().available_ns, policy.budget_ns);
        assert!(super::super::scheduling_runtime::prepare_published_stamp_dispatch(
            stamp, 0, policy.period_ns + 1,
        ));
        let stale_context = super::super::super::current_identity::PagerChargeStamp {
            context_generation: stamp.context_generation.wrapping_add(1),
            ..stamp
        };
        assert!(super::super::scheduling_runtime::charge_published_stamp_pair(
            stale_context,
            0,
            policy.period_ns + 1,
            policy.budget_ns,
            76,
        )
        .is_none());
        let stale = super::super::super::current_identity::PagerChargeStamp {
            domain_generation: stamp.domain_generation.wrapping_add(1),
            ..stamp
        };
        assert!(super::super::scheduling_runtime::charge_published_stamp_pair(
            stale,
            0,
            policy.period_ns + 1,
            policy.budget_ns,
            77,
        )
        .is_none());
        assert_eq!(context.runtime_snapshot().unwrap().available_ns, policy.budget_ns);
        let (context_charge, domain_charge) =
            super::super::scheduling_runtime::charge_published_stamp_pair(
                stamp,
                0,
                policy.period_ns + 1,
                policy.budget_ns,
                77,
            )
            .expect("exact published charge");
        assert!(context_charge.exhausted && domain_charge.exhausted);
        let budget = context.runtime_snapshot().unwrap();
        assert_eq!(budget.available_ns, 0);
        assert_eq!(budget.timeout_fault_count, 1);
        assert_eq!(budget.timeout_fault_reply, 77);
        assert!(!super::super::scheduling_runtime::published_stamp_is_eligible(stamp, 1, 0));
        assert!(
            !super::super::scheduling_runtime::published_stamp_is_eligible(stamp, usize::MAX, 0)
        );
    }
