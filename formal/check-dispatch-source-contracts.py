#!/usr/bin/env python3
"""Actual independent dispatch refinement; never certify a legacy fallback.

These anchors complement the physical native witnesses and mutations. They do
not certify MM/register/RSP hardware or the full joint terminal/reuse boundary.
"""
from pathlib import Path
import re

base = Path('kernel/ps/src/multitask/scheduler')
dispatch = (base / 'dispatcher.rs').read_text()

# The genuine global guard and every consumer are deleted, not cfg-hidden.
# Whole-type/backend deletion is checked below, separately from this guard ban.
for source in Path('kernel/ps/src').rglob('*.rs'):
    text = source.read_text()
    if re.search(r'\b(?:static\s+SCHEDULER\s*:|struct\s+SchedulerAccessGuard\b|fn\s+scheduler_(?:mut|ref)\s*\(|scheduler_(?:mut|ref)\s*\()', text):
        raise SystemExit('deleted global scheduler authority returned: ' + str(source))
for source in ['kernel/ps/src/api.rs', 'kernel/ps/src/multitask/mod.rs',
        'kernel/ps/src/multitask/current.rs']:
    if 'any_user_process_state' in Path(source).read_text():
        raise SystemExit('unbound global process catalog snapshot returned: ' + source)


def body(source, name):
    matches = list(re.finditer(r'\bfn\s+' + re.escape(name) + r'(?:<[^{}]*?>)?\s*\(', source))
    if len(matches) != 1:
        raise SystemExit('missing/ambiguous dispatch owner: ' + name)
    start = matches[0].start()
    # Preserve offsets while ignoring comments/literals containing braces.
    lexical = re.sub(r'"(?:\\.|[^"\\])*"|//[^\n]*|/\*[\s\S]*?\*/',
                     lambda match: ' ' * len(match[0]), source)
    opening = lexical.index('{', start)
    depth = 1
    for end in range(opening + 1, len(lexical)):
        depth += (lexical[end] == '{') - (lexical[end] == '}')
        if depth == 0:
            return source[start:end + 1]
    raise SystemExit('unclosed dispatch owner: ' + name)


def ordered(source, anchors, boundary):
    source = ' '.join(source.split())
    after = 0
    for anchor in anchors:
        anchor = ' '.join(anchor.split())
        found = source.find(anchor, after)
        if found < 0:
            raise SystemExit(boundary + ' lost ordered effect: ' + anchor)
        after = found + len(anchor)


current_ipc = Path('kernel/ps/src/multitask/current/ipc.rs').read_text()
ordered(body(current_ipc, 'settle_reply_scheduling_context'), [
    'DispatchPreemption::enter()', 'scope.mask_irqs()', 'reply_context_custody(owner_task_id, identity)',
    'classify_unpublished_reply_custody(matches, live)', 'release_reply_donation('],
    'one original masked CPU encloses reply attribution and exact ledger release')
ordered(body(current_ipc, 'settle_terminal_ipc_reply_scheduling_context'), [
    'settle_reply_scheduling_context(reply, custody)',
    '!= ReplySchedulingContextSettlement::IdentityMismatch'],
    'terminal completion/cancel permits retired original owner but not live foreign context')
ordered(body(current_ipc, 'complete_ipc_reply_wake_handoff_with_custody'), [
    'custody.caller_task_id()', 'completion.caller_task_id',
    'settle_terminal_ipc_reply_scheduling_context(reply, custody)',
    'complete_ipc_reply_wake_handoff(reply, completion.caller_task_id)'],
    'late slow reply still uses exact original typed caller wake, never replacement funding')

# A per-slot stable empty record is not a committed unmap while a whole range
# rewrite withdraws/moves/replaces its original publications. These are bounded
# observations only, not a new process/frame grant or an IRQ-off spinning lock.
pager_vma = Path('kernel/ps/src/multitask/pager_vma.rs').read_text()
ordered(body(pager_vma, 'lookup_slot'), ['read_rewrite_epoch(handle, ||',
    'for (index, slot)', 'slot.may_cover(address)', 'slot.snapshot()?',
    'region_matches_identity(', 'admit_access(region, access)'],
    'complete VMA scan is enclosed by one whole-rewrite observation')
rewrite_epoch = Path('kernel/ps/src/multitask/pager_vma/rewrite_epoch.rs').read_text()
ordered(pager_vma, ['mod rewrite_epoch;', 'use rewrite_epoch::{RangeRewrite, read_rewrite_epoch};'],
    'actual lookup links the split whole-rewrite reader and original writer interval')
ordered(body(rewrite_epoch, 'read_rewrite_epoch'), ['load(Ordering::Acquire)',
    'before & 1 != 0', 'PagerVmaError::Unstable', 'let result = scan()',
    'fence(Ordering::Acquire)', 'sequence.load(Ordering::Relaxed) != before',
    'PagerVmaError::Unstable', 'result'], 'VMA fault scan retries, never trusts a temporary hole')
ordered(body(pager_vma, 'rewrite_range'), ['let _rewrite = RangeRewrite::begin(handle)?',
    'slots[slot_index].publish(None)', 'if let Err(error) = mutate()',
    'slots[targets[index]].publish(Some(rewritten[index]))'],
    'whole-rewrite interval survives withdrawal, PTE callback and all replacement/rollback effects')
ordered(body(Path('kernel/ps/src/multitask/current/pager.rs').read_text(),
    'published_current_pager_binding'), ['published_current_identity()',
    'identity.user_binding()?', 'owned_current_user_binding()?',
    'published_live_process_identity(handle)'],
    'overlapping current identity publication retains only original Current-bank binding')

# Shared ordinary byte/handle RPC waits refine ArmBlock with the SAME reply
# handle. A generic wait cannot be completed by the exact typed reply producer.
rpc_wait = body(Path('kernel/compat/src/user/syscall/linux/ipc_ops/reply_wait.rs').read_text(),
    'wait_for_reply_with_deadline')
ordered(rpc_wait, ['take_endpoint_response_for_wait(reply, handle_capacity)',
    'multitask::arm_block_current_task_on_reply(reply.raw())', 'arm_reply_deadline_waiter(',
    'take_endpoint_response_for_wait(reply, handle_capacity)',
    'multitask::commit_block_current_task_and_yield()'], 'exact typed ordinary RPC reply wait')
if 'multitask::arm_block_current_task()' in rpc_wait:
    raise SystemExit('ordinary RPC reply reverted to an unwakeable Generic wait')

# Deferred registry permission is transferred AFTER physical preflight, never
# retained as a raw guard across Current/Cohort capture or queue effects.
activation_commit = body((base / 'activation/publication.rs').read_text(), 'commit')
ordered(activation_commit, ['OriginalCurrent::borrow(turn)', 'let members =',
    'frame_readers::acquire(', 'readers[i] = Some(reader)',
    'let Some(authority) = commit_authority() else { return false; };',
    'held_spin_lock_depth()', 'owner.cohort.begin_effects()', 'owner.cohort.unsuspend(slot, id)',
    'owner.cohort.complete()', 'drop(readers)', 'drop(pins)', 'current.check()', 'drop(authority)'],
    'original activation grant survives final publication without raw registry lock')
proc_broker = Path('kernel/compat/src/user/syscall/linux/proc_broker_ops.rs').read_text()
for text, name in [(proc_broker, 'syscall_linux_rustos_proc_activate_broker'),
        (Path('kernel/compat/src/user/syscall/linux/proc_broker_ops/activation_batch.rs').read_text(),
         'syscall_linux_rustos_proc_activate_batch_broker')]:
    ordered(body(text, name), ['activate_suspended_user_tasks_with_commit(',
        'let mut activations = DEFERRED_ACTIVATIONS.lock()', 'deferred_activation::take_exact::<',
        'drop(activations)', 'grant'], 'original permission transfer callback: ' + name)
if re.search(r'#\[derive\([^\]]*\b(?:Copy|Clone)\b[^\]]*\)\]\s*struct DeferredActivationAuthority', proc_broker):
    raise SystemExit('deferred activation permission became copyable')
original_grant = Path('kernel/compat/src/user/syscall/linux/proc_broker_ops/deferred_activation.rs').read_text()
ordered(body(original_grant, 'take_exact'), ['targets.is_empty()', 'targets.len() > N',
    'targets[..index].contains(&pid)', 'registry.get(&pid).is_none_or', 'return None',
    'let mut grant', 'registry.remove(pid)', 'Some(grant)'], 'whole batch original grant transfer')

# Atomic first turn and synchronous FIFO must finish/refuse before overdue,
# spawn and later policy arms; an incomplete arm never authorizes a later peer.
choose = body(dispatch, 'choose')
ordered(choose, ['dispatch_policy::atomic_activation_pending(cpu)',
    'first_turn(turn, cpu, true, class_sequence)', 'result => return result',
    'synchronous(cpu)', 'result => return result',
    'local_dispatch::retry_incomplete_policy(|| local_dispatch::choose_overdue_system(',
    'runqueue::local_runnable_sequence(cpu)', 'donation_ledger::class_publication_sequence()',
    'slot, id, generation', 'Err(_) => return Decision::Incomplete',
    'first_turn(turn, cpu, false, class_sequence)', 'latency(cpu)'], 'complete early priority')
ordered(choose, ['local_dispatch::user_reservation_due(cpu)',
    'let class_sequence = donation_ledger::class_publication_sequence()',
    'class_sequence & 1 != 0', 'return Decision::Incomplete',
    'retry_fair(cpu, current, voluntary, runtime)',
    'local_dispatch::observed_class(candidate.slot)',
    'donation_ledger::class_publication_sequence() != class_sequence',
    'return Decision::Incomplete', 'Ok(local_dispatch::LocalClass::User) => return Decision::Selected(candidate)',
    'Err(_) => return Decision::Incomplete', 'match hint(cpu, current)',
    'match retry_fair(cpu, current, voluntary, runtime)'], 'due User reservation')
ordered(body(dispatch, 'fair'), ['local_dispatch::choose_local_next(',
    'runqueue::local_runnable_sequence(cpu)', 'donation_ledger::class_publication_sequence()',
    'Ok((slot, generation))', 'current_identity::read_dispatch(slot)',
    'Candidate { slot, id, generation'], 'complete generation-bound fair observation')
ordered(body(dispatch, 'retry_fair'), ['local_dispatch::retry_incomplete_policy(|| fair('],
    'shared bounded fair retry')
ordered(body(dispatch, 'dispatch'), ['turn.begin_priority_choice()',
    'prologue(outgoing, rsp, voluntary)', 'match choose(', 'Decision::Selected(candidate)',
    'if claim(outgoing, candidate, class_sequence)', 'Decision::Claimed(urgency)',
    'Decision::Empty | Decision::Incomplete', 'smp::idle_slot_for_cpu(cpu)',
    'dispatch_claim::claim_fallback(turn, idle, class_sequence)'], 'original choice and incomplete fallback')
if any(forbidden in dispatch for forbidden in ['scheduler_mut(', 'scheduler_ref(',
        'catalog_fair_fallback(', 'dispatch_schedule(']):
    raise SystemExit('independent dispatch regained legacy authority')
# Old final validators are deleted, including their native unit-Ready grants.
if (base / 'local_dispatch/selected_frame.rs').exists():
    raise SystemExit('obsolete selected-frame authority returned')
local = (base / 'local_dispatch.rs').read_text()
for name, final_token in [('candidate_is_qualified', 'token_is_current'),
        ('direct_handoff_frame_generation', 'direct_handoff_token_is_current')]:
    selected = body(local, name)
    ordered(selected, ['pin_allocation(frame)', 'frame_readers::acquire(&_allocation_pin, cpu)',
        'validate_published_saved_context(', final_token + '(frame, cpu)'],
        'native and production original selection reader: ' + name)
    if '#[cfg' in selected or 'cfg!' in selected:
        raise SystemExit('selection lifetime is conditional: ' + name)
for deleted in ['validate_selected_local_frame', 'validate_selected_direct_frame',
        'validate_selected_current_frame', 'qualified_current_generation',
        'SelectedPublishedFrame', 'claim_validated_idle_fallback']:
    for source in base.rglob('*.rs'):
        if re.search(r'\b(?:fn|enum)\s+' + deleted + r'\b', source.read_text()):
            raise SystemExit('obsolete final frame grant returned: ' + deleted)
# The catalog fair/reservation/age/generic hint algorithms are physically
# deleted, not cfg-hidden. Historical policy witnesses use actual born backing.
legacy_policy = (base.parent / 'scheduler.rs').read_text() + (base / 'locality.rs').read_text()
for deleted in ['candidate_is_local_to_current_cpu', 'prefer_local_candidate',
        'pick_min_vruntime', 'pick_min_vruntime_inner', 'pick_min_vruntime_excluding',
        'pick_min_vruntime_excluding_inner', 'pick_min_vruntime_in_class',
        'profile_yield_admission', 'maybe_keep_current', 'pick_burst_alternate_in_current_class',
        'reserved_user_pick', 'reserved_user_pick_inner', 'overdue_class_pick',
        'overdue_class_pick_inner', 'overdue_system_pick', 'mandatory_overdue_system_pick',
        'record_dispatch_streaks', 'account_current_runtime', 'pick_hint_ready_slot',
        'take_next_pick_hint_ready_slot']:
    if re.search(r'\bfn\s+' + deleted + r'\s*\(', legacy_policy):
        raise SystemExit('legacy policy authority returned: ' + deleted)
if re.search(r'\bfn\s+user_reservation_due\s*\(', (base.parent / 'scheduler.rs').read_text()):
    raise SystemExit('legacy policy reservation authority returned')
policy = (base / 'dispatch_policy.rs').read_text()
ordered(body(dispatch, 'complete'), ['turn.claimed_return()',
    'dispatch_policy::discharge_claimed_first_turn(turn)', 'dispatch_policy::record_completed_choice(cpu,'],
    'all priority arms discharge only the original successful first turn')
ordered(body(policy, 'discharge_claimed_first_turn'), ['turn.original_current()',
    'turn.owns_priority_choice()', 'turn.claimed_return()',
    'FirstTurnHandoff { slot: receipt.slot(), task_id: receipt.id }', 'let mut policy = lock(cpu)',
    'policy.spawn_pick_hints.remove_exact(record)',
    'policy.atomic_activation_pick_hints.remove_exact(record)', '.checked_sub(1)',
    'policy.atomic_activation_pick_hints.is_empty()', 'clear_atomic_activation_pending(cpu)'],
    'exact first-turn completion retains original physical claim and prefix accounting')
ordered(body((base / 'handoff_queue.rs').read_text(), 'remove_exact'),
    ['== Some(entry)', 'else { return false; }', 'for next in offset + 1..self.len',
     'self.slots[to] = self.slots[from]', 'self.len -= 1'],
    'exact incarnation withdrawal preserves FIFO suffix without capacity-sized scratch')
ordered(body(dispatch, 'complete'), ['turn.claimed_return()', 'dispatch_policy::record_completed_choice(cpu,'],
    'actual claim before independent completed-choice counters')
ordered(body(policy, 'record_completed_choice'), ['if system_class', 'system_dispatch_streak(cpu)',
    'MAX_CONSECUTIVE_SYSTEM_DISPATCHES', 'if latency_handoff', 'latency_handoff_streak(cpu)',
    'MAX_CONSECUTIVE_LATENCY_HANDOFFS', 'store_dispatch_streaks(cpu, system, latency)'],
    'single independent completed-choice policy transition')
if '#[cfg' in body(policy, 'record_completed_choice'):
    raise SystemExit('completed-choice policy regained a native-only algorithm')
policy_witness = (base / 'local_dispatch/policy_witnesses.rs').read_text()
for required in ['irq_entry_tests::current_with_peer(true', 'thread_creation::tests::active_sibling(',
        'CurrentReturn::capture(entry.turn())', 'outgoing.account(', 'outgoing.publish_frame(',
        'outgoing.publish_queue(true)', 'choose_local_next(', 'choose_overdue_system(',
        'dispatcher::test_policy_choice(entry.turn()', 'thread_creation::tests::release_born(']:
    if required not in policy_witness:
        raise SystemExit('actual policy ownership witness lost: ' + required)
# Both fixed-idle and original Current fallback retain the same exact physical
# frame, generation, funding and final CAS; empty is not an unvalidated idle hint.
claim = (base / 'dispatch_claim.rs').read_text()
ordered(body(claim, 'claim_fallback'), ['let (cpu, original) = turn', '.original_current()',
    'frame_publication::current_local_frame_token(original, cpu)',
    'DispatchFrame::capture_selected(', 'token.owner_generation', 'FrameCustody::Current', 'class_sequence,',
    'frame.prepare().is_some_and(FundedFrame::claim)', 'runqueue::is_idle_slot(idle_slot)',
    'frame_publication::local_frame_token(idle_slot, cpu)', 'DispatchFrame::capture(',
    'token.owner_generation', 'FrameCustody::Local', '.prepare()',
    'funded.funding.funding.is_none()', 'funded.claim()'], 'exact Current/fixed idle fallback')
# FIFO Direct observations carry their consumed owner generation, never a new
# relookup grant. Refused claim materializes only that exact generation to fair.
ordered(body(dispatch, 'materialize'), ['record.claim_generation()',
    'materialize_direct_handoff_with_generation(record.slot()', 'Some(generation)'],
    'consumed Direct refusal')
ordered(body(dispatch, 'synchronous'), ['record.has_current_dispatch_custody()',
    'local_dispatch::qualified_direct_handoff_generation(',
    'record.bound_direct_generation().is_some_and(|expected| expected != g)',
    'record.bind_selected_local_owner(runqueue::owner(record.slot()), cpu)',
    'record.claim_generation()', 'id: record.task_id(), generation'],
    'exact Direct FIFO selection')
ordered(body(dispatch, 'claim'), ['frame_publication::direct_handoff_frame_token(',
    'DispatchFrame::capture_selected(', 'candidate.id', 'candidate.generation',
    'FrameCapture::Ready(frame) => frame.prepare().is_some_and(|funded| funded.claim())',
    'materialize_direct_handoff_with_generation(candidate.slot', 'Some(candidate.generation)'],
    'exact Direct original physical claim')
sync = (base / 'sync_handoff.rs').read_text()
sync_consumer = body(sync[sync.index("/// Consumes this CPU's FIFO"):], 'take_next_ready')
ordered(sync_consumer, ['SYNC_HANDOFF_STREAK[cpu]', 'for attempt in 0..MAX_TASK',
    'let taken = {', 'state_for_cpu(cpu).lock()',
    'take_next_ready_record_with_competitor(false, |_| true)',
    'if state.len == 0', 'pending.store(false,', 'taken\n        };',
    'let Some(record) = taken', 'if ready(record)', 'return Some(record)'],
    'bounded exact FIFO pop ends raw leaf before original frame qualification')
ordered(body(sync, 'enqueue_and_publish'), ['state.enqueue_with_direct_owner(record, Some((owner, target_cpu)))'],
    'refresh stale Direct record')
if not any(anchor in sync for anchor in ['record.bind_direct_owner(owner, cpu)',
        'record.bind_direct_owner(owner, target_cpu)']):
    raise SystemExit('Direct publication lost exact generation binding')
for anchor in ['owner.state == runqueue::RunOwnerState::DirectHandoff',
        '&& owner.generation == owner_generation']:
    if anchor not in sync:
        raise SystemExit('Direct FIFO custody lost ' + anchor)
owner = (base / 'runqueue/ipc_handoff.rs').read_text()
ordered(body(owner, 'publish_direct_handoff'), [
    'OWNER_WORDS[slot].compare_exchange(owner, next).is_ok()',
    'frame_publication::bind_local_owner_generation(slot, next.generation)'],
    'Direct CAS before frame generation binding')
frame = (base / 'frame_publication.rs').read_text()
for anchor in ['fn direct_handoff_frame_token(', 'fn direct_handoff_token_is_current(']:
    if anchor not in frame:
        raise SystemExit('Direct physical qualification lost ' + anchor)
irq = Path('kernel/ps/src/multitask/irq.rs').read_text()
ordered(body(irq, 'software_schedule_interrupt_dispatch'), [
    'DispatchEntry::enter()', 'entry.save_simd()',
    'scheduler::ipc_publication::call(',
    'entry.preemption()',
    'scheduler::ipc_publication::reply(',
    'entry.preemption()',
    'immediate_task_id = outcome.dispatch_hint(caller_task_id)',
    'entry.dispatch(current_rsp, true, immediate_task_id)',
    'entry.install(dispatch, exact_fast_dispatch)'],
    'actual fast producers cannot bypass the sole Direct FIFO')
for entry_name in ['timer_interrupt_dispatch', 'rtc_interrupt_dispatch',
        'reschedule_ipi_interrupt_dispatch', 'software_schedule_interrupt_dispatch',
        'saved_user_context_dispatch']:
    actual_entry = body(irq, entry_name)
    ordered(actual_entry, ['DispatchEntry::enter()', 'runtime_profile::record_entry(',
        'runtime_profile::take_owned(entry.turn()', 'entry.dispatch(', 'entry.install('],
        'actual independent IRQ ' + entry_name)
    if re.search(r'scheduler_(?:mut|ref)\(|SchedulerAccessGuard|release_dispatch_directory', actual_entry):
        raise SystemExit('IRQ entry regained directory authority: ' + entry_name)
entry = Path('kernel/ps/src/multitask/cpu_local/irq_entry.rs').read_text()
release_witness = Path('kernel/ps/src/multitask/cpu_local.rs').read_text()
ordered(body(release_witness, 'scheduler_guard_release_flushes_deferred_target_notification'),
    ['irq_entry_tests::current(', 'DispatchPreemption::enter()', 'scope.mask_irqs()',
     'DispatchEntry::from_scope(scope)', 'prepare_test_deferred_target_reschedule(target_cpu)',
     'drop(entry)', 'test_deferred_target_reschedule_pending_mask()',
     'test_deferred_target_reschedule_sent_mask()', 'test_deferred_target_reschedule_flush_epoch()'],
    'historical global-release obligation uses actual independent physical IRQ owner')
ordered(body(entry, 'from_scope'), ['scope.is_masked()', 'scheduler_initialized()',
    'cpu_task_slot_admitted(cpu)', 'CURRENT_TASK_SLOTS[cpu].load(Ordering::Acquire)',
    'dispatch_turn::begin(scope, from)', 'turn.original_current().is_some()',
    'deferred_wake::take_masked(entry.preemption())'], 'original IRQ admission and own deferred mailbox')
ordered(body(entry, 'dispatch'), ['self.save_simd()', 'CurrentReturn::capture(self.turn())',
    'dispatcher::fast(&mut outgoing', 'dispatcher::dispatch(&mut outgoing'],
    'one original Current SIMD and failed fast fallback')
ordered(body(entry, 'accept_installed_return'), ['turn.architecture_ready()',
    'execution_interval::original_claim_is_active(turn)', 'turn.claimed_return()',
    'receipt.returns_to_user()', 'receipt.slot()', 'self.selected = Some('],
    'actual original architecture and executing interval acceptance')
ordered(body(entry, 'drop'), ['self.turn.take()', 'self.selected.unwrap_or_else(',
    'turn.original_current()', 'turn.publish_return(', 'flush_deferred_target_reschedules()',
    'completion.after_directory_unlock()'], 'original receipt selection and parked RSP return')
if re.search(r'scheduler_(?:mut|ref)\(|SchedulerAccessGuard|Deref|current_task_scratch', entry):
    raise SystemExit('independent IRQ entry regained legacy or scratch authority')
maintenance = (base / 'dispatcher/maintenance.rs').read_text()
ordered(body(maintenance, 'mobile'), ['slot != ROOT_TASK_SLOT',
    '!runqueue::is_idle_slot(slot)', '!crate::multitask::task_slot_is_running(slot)',
    'runqueue::is_local_dispatchable(slot, source)',
    'runqueue::weight::observe(slot).is_some()', 'p.lifecycle_dispatchable()'],
    'migration excludes every published execution owner')
ordered(body(dispatch, 'choose'), ['class_sequence & 1 != 0',
    'runqueue::drain_remote_wakes(cpu)', 'dispatch_policy::atomic_activation_pending(cpu)',
    'first_turn(turn, cpu, true, class_sequence)', 'match synchronous(cpu)',
    'maintenance::service(turn, cpu, current, voluntary)'],
    'remote mailbox admission precedes every priority early return, not deferred balance')
ordered(body(maintenance, 'service'), ['turn.owns_priority_choice()',
    '!idle && !voluntary && runqueue::published_runnable_count(cpu) >= 2',
    'OPPORTUNITIES[cpu].fetch_add(1, Ordering::Relaxed)',
    'runqueue_policy::active_balance_opportunity_due(previous)',
    'if !mobile(slot, cpu)', 'runqueue::least_loaded_cpu(',
    'candidate(slot, cpu, target)', 'transfer(turn, slot, source, target)'],
    'loaded-opportunity balance, never RTC residue')
ordered(body(maintenance, 'transfer'), ['frame_publication::pin_for_identity(token, id)',
    'frame_readers::acquire(&pin, cpu)', 'context_validation::validate_published_saved_context(',
    'MigrationFrame { turn, token, id, source, _reader: reader }',
    'runqueue::rehome_queued_frame(&frame', 'drop(frame)', 'drop(pin)',
    'request_target_reschedule(cpu)'], 'original migration reader through mailbox, outside-leaf IPI')
ordered(body(owner, 'rehome_queued_frame'), ['owner.generation != frame.token.owner_generation',
    'RUN_QUEUES[source_cpu].inner.lock()', '!source.contains(slot) || !frame.is_current()',
    'with_rehome_admission(slot, target_cpu, None', 'if !frame.is_current()',
    'compare_exchange(owner, migrating)', 'source.remove(slot, weight)',
    'publish_load(&source)', 'drop(source)',
    'publish_migrating_record(slot, migrating, target_cpu, weight)'],
    'exact source-generation migration CAS and mailbox')
# Whole fixed-fault exception producer and resumed Current architecture. Native
# capture witnesses cannot execute the blocking trap or privileged registers;
# enforce original custody/order at the actual production caller as well.
pager = (base / 'pager_handoff.rs').read_text()
for obsolete in ['handoff_pager_fault_to_waiter', 'apply_blocked_pager_donation',
                 'refresh_current_user_return_architecture']:
    if any(obsolete in path.read_text() for path in base.rglob('*.rs')):
        raise SystemExit('pager regained legacy authority: ' + obsolete)
if 'scheduler_mut(' in irq or 'scheduler_ref(' in irq:
    raise SystemExit('actual IRQ still acquires legacy Scheduler')
ordered(body(irq, 'commit_pager_fault_block_and_yield'), [
    'scope.mask_irqs()', 'CurrentWait::capture()', 'wait.commit()',
    'mark_pager_fault_blocked(token)', 'wait.restore_pager(token)',
    'pager_handoff::handoff(&scope, &wait, token, worker_id)',
    'drop(wait)', 'drop(scope)', 'trigger_software_schedule()',
    'resumed.mask_irqs()', 'CurrentWait::capture()',
    'CurrentArchitecture::capture(&resumed, &current)', '.install()',
    'drop(current)', 'drop(resumed)'], 'whole original pager ingress/trap/resume')
ordered(body(pager, 'handoff_with'), ['!sender.committed_pager(token)',
    'Binding::resolve(worker_id)', 'retained_wake_matching(worker,',
    'Some((runqueue::wait::REASON_PAGER_SERVICE, 0))',
    'latency_floor(sender, token, worker, scope.cpu())',
    'TypedWake::capture(&worker,', 'REASON_PAGER_SERVICE, 0',
    'ipc_publication::eligible(&worker, target)',
    'ipc_publication::publish_with(scope, &worker, &typed, target,',
    'latency_floor(sender, token, &worker, target)', 'drop(typed)', 'drop(worker)'],
    'exact original service worker, no generic fallback')
ordered(body(pager, 'latency_floor'), ['sender.original_pin()', 'worker.original_pin()',
    '!sender.committed_pager(token)', 'ready_floor(cpu,', 'lower_vruntime_ceiling('],
    'same physical donor/worker through latency effect')
producer = (base / 'ipc_publication.rs').read_text()
ordered(body(producer, 'call_before_arm_with'), ['call_before_arm_observing(',
    'eligible_cpus, || {}, last_effect)'], 'production uses same raced receive engine')
ordered(body(producer, 'call_before_arm_observing'), [
    'CurrentWait::capture()', 'donation_publication::participant(sender.id())',
    'reply_already_dispatched(sender.original_pin(), reply)',
    'wake_publication::Binding::from_original', 'TypedWake::receive_was_withdrawn(&receiver)',
    'donation_ledger::retains_awaiting_reply(donor.as_ref().unwrap(), reply)',
    'last_effect.take().unwrap()()', 'inherited: false, woke: false, hinted: false',
    'before_receive()', 'wake_publication::receive_call(receiver, endpoint,'],
    'withdrawn receive retains only exact original queued custody, never grants worker execution')
ordered(body(producer, 'publish_with'), ['frame_readers::acquire(&pin, cpu)',
    'Publication::begin(scope, target)', 'sync_handoff::publish_original(',
    'typed.publish_and_clear(', 'claim_blocked_handoff(slot, expected, target, local)',
    'bind_local_owner_generation(slot, original.generation)',
    'publish_handoff_mailbox(slot, original,', 'last_effect()',
    'drop(publication)', 'drop(reader)', 'drop(pin)'], 'reader/phase/FIFO through pager floor')
architecture = (base / 'return_architecture.rs').read_text()
if "current: &'a super::CurrentWait" not in architecture or "scope: &'a" not in architecture:
    raise SystemExit('resumed architecture lost borrowed original Current/mask')
ordered(architecture[architecture.index("impl<'a> CurrentArchitecture"):], [
    '!super::ipc_publication::scoped(scope)', 'current.original_pin()',
    'return_view(pin.slot())', '!view.user_mode_task', 'retained_architecture(',
    'pin.slot(), pin.id(), view', 'Some(Self { current, scope, state })',
    'fn install(self)', 'self.current.cpu()', 'self.scope.is_masked()',
    'load_address_space_phys(PhysAddr::new(self.state.root))',
    'set_privilege_stack(self.state.stack_top)', 'set_kernel_stack_top(self.state.stack_top)',
    'install_segments(self.state, true)'], 'retained Current MM/stack through final registers')
# Native born-resource returns use exactly the production resource qualification
# and both bank transitions. Only explicitly constructed integer phase models
# may omit them; an actual successful claim can never enter that projection.
turn_source = (base.parent / 'cpu_local/dispatch_turn.rs').read_text()
publication = body(turn_source, 'publish_return')
ordered(publication, ['if !self.cell.models_only_phases()', 'if self.from != to',
    'changed return lacks placement grant', 'self.architecture_phase.get() == 2',
    'execution_interval::finish_return(&self, to)',
    'phase model cannot publish a physical claim', 'self.seal()',
    'publication.publish_before_return()', 'choice.published()'], 'strict original resource publication')
for fragment in ['#[cfg(not(test))] if self.from', '#[cfg(not(test))] if let Some(receipt)',
                 '#[cfg(not(test))] super::super::scheduler::execution_interval']:
    if fragment in ' '.join(publication.split()):
        raise SystemExit('real native return skips production resource qualification')
ordered(body(architecture, 'install_with'), ['self.turn.begin_architecture()',
    'if self.changed', 'effect(ArchitectureEffect::AddressSpace(next.root))',
    'effect(ArchitectureEffect::PrivilegeStack(next.stack_top))',
    'effect(ArchitectureEffect::SyscallStack(next.stack_top))',
    'fast_ipc && previous.root == next.root && !self.receipt.return_to_user',
    'effect(ArchitectureEffect::Segments(next, self.receipt.return_to_user))',
    'effect(ArchitectureEffect::Simd(self.receipt.token.slot))',
    'self.turn.mark_architecture_ready()'], 'same architecture state machine through last effect')
# install has three independently typed owners; restrict to the return owner.
original_install = architecture[architecture.index("impl<'a> ReturnArchitecture"):architecture.index('/// Scalar descriptions')]
ordered(body(original_install, 'install'), ['self.install_with(fast_ipc, hardware_effect)'],
    'production claimed architecture uses shared state machine')
cpu_source = (base.parent / 'cpu_local.rs').read_text()
ordered(body(cpu_source, 'publish_before_return'), ['execution::banks::publish(',
    'cpu_leases::publish(', 'self.publish_scalars()'], 'both original banks before scalar publication')
ordered(body(cpu_source, 'commit_context_switch'), ['asm!("mov {}, rsp"',
    'let observed_rsp = Some(rsp)', 'commit_context_switch_at_rsp(logical_index, observed_rsp)'],
    'hardware callback observes actual machine RSP before shared ACK algorithm')
ordered(body(cpu_source, 'commit_context_switch_at_rsp'), ['dispatch_turn::begin_commit(',
    'commit.assert_arrived(logical_index, rsp)',
    'TRANSITION_ACTIVE[logical_index].store(false', 'commit.physical_banks()',
    'cpu_leases::commit(logical_index)', 'execution::banks::commit(logical_index)',
    'commit.finish()'], 'checked actual RSP precedes both original bank ACKs and reopening')
rsp_handoff = (base / 'rsp_handoff.rs').read_text()
ordered(body(rsp_handoff, 'capture'), ['turn.original_current()', 'turn.claimed_return()',
    'frame_publication::return_view(from)', 'current_return_binding(cpu, from, old)',
    'owns_prepared(cpu, to)', 'receipt.token.saved_rsp', 'receipt.token.view'],
    'RSP seal borrows original outgoing and claimed incoming allocation geometry')
ordered(body(rsp_handoff, 'assert_arrived'), ['self.cpu == cpu', 'self.incoming.contains(rsp)',
    '!self.outgoing.contains(rsp)', 'cpu_leases::owns_current(cpu, self.to)',
    'cpu_leases::owns_outgoing(cpu, self.from)', 'execution::banks::owns_current(cpu, self.to)',
    'execution::banks::owns_outgoing(cpu, self.from)'], 'RSP and both exact original bank roles')
ordered(publication, ['let handoff = RspHandoff::capture(&self)', '*bank = Some(handoff)',
    'execution_interval::finish_return(&self, to)', 'self.seal()', 'publication.publish_before_return()'],
    'original producer seals geometry before publication and bank transfer')
# Joint final admission is not the earlier independent funding observation.
# Each leaf borrows the SAME non-Copy grant; all leaves end before CPU banks.
ordered(body(dispatch, 'dispatch'), ['prologue(outgoing, rsp, voluntary)',
    'let class_sequence = donation_ledger::class_publication_sequence()',
    'match choose(', 'class_sequence)', 'claim(outgoing, candidate, class_sequence)'],
    'original preselection class through claim')
ordered(body(dispatch, 'first_turn'), ['DispatchFrame::capture_selected(',
    'FrameCustody::Local, class_sequence)', 'funded.claim()',
    'policy.atomic_activation_pick_hints.pop()'], 'first-turn original class before FIFO pop')
ordered(body(claim, 'capture_selected'), ['class_sequence: u64',
    'frame_publication::pin_for_identity(token, id)', 'frame_readers::acquire(&pin, cpu)',
    'drop(reader)', 'FrameCapture::Ready(Self', 'class_sequence,', 'pin,'],
    'original class observation qualified by original physical frame')
ordered(body(claim, 'with_admission'), ['runqueue::is_idle_slot(self.token.slot)',
    'if funding.is_some() { return None; }', 'return claim()', 'with_class_claim(self.class_sequence,',
    'match funding', 'Some(pin) => pin.with_dispatch_admission(self.cpu,', 'None => claim()'],
    'class45 before original funding48/49')
ledger = (base / 'donation_ledger.rs').read_text()
ordered(body(ledger, 'with_class_claim'), ['DONATION_LEDGER.lock()',
    'expected & 1 != 0 || class_publication_sequence() != expected', 'return None', 'claim()'],
    'original class leaf through bounded claim')
ordered(body(ledger, 'publish_base_class'), ['DONATION_LEDGER.lock()',
    'CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel)', 'effect()',
    'CLASS_PUBLICATION_SEQUENCE.fetch_add(1, Ordering::AcqRel)'], 'live base-class producer join')
runtime = (base / 'scheduling_runtime.rs').read_text()
ordered(body(runtime, 'with_dispatch_admission'), ['with_published_budget_pair(self.stamp, cpu,',
    '!context.is_eligible(now_ns) || !domain.is_eligible(now_ns)', 'return None', 'claim()'],
    'original pinned budget admission and claim in SAME leaf callback')
queue_claim = (base / 'runqueue/claim.rs').read_text()
ordered(body(queue_claim, 'claim_dispatch_inner'), ['RunOwnerState::DirectHandoff',
    'admitted_claim(frame, funding,', 'with_claim_activation(slot, cpu,',
    'frame.is_current()', 'compare_exchange(owner, owner.next(',
    'retain_mm_activation(cpu, activation)', 'RUN_QUEUES[cpu].inner.lock()',
    'admitted_claim(frame, funding,', 'with_claim_activation(slot, cpu,',
    'frame.is_current()', 'compare_exchange(owner, next)', 'drop(rq)',
    'retain_mm_activation(cpu, activation)'], 'rq40 before admission and exact CAS before bank publication')
frame_readers = (base / 'stack_resources/frame_readers.rs').read_text()
ordered(body(frame_readers, 'with_claim_activation'), [
    'execution::with_claim', 'RESOURCES.get(slot)?', 'activation::capture',
    'affinity_payload::with_placement_admission', 'Some((claimed, activation))',
    'Some((None, activation)) => Some(ClaimAttempt::Refused { activation, permit })',
    'None => Some(ClaimAttempt::Refused { activation: None, permit })'],
    'original refused MM/UID loans leave admission callback without Drop')
if 'drop(' in body(frame_readers, 'with_claim_activation'):
    raise SystemExit('provisional refusal Drop regained enclosing admission leaves')
ordered(body(queue_claim, 'claim_dispatch_inner'), [
    'admitted_claim(frame, funding,', '.and_then(|attempt| attempt.into_claimed())',
    'RUN_QUEUES[cpu].inner.lock()', 'let Some(attempt)',
    'admitted_claim(frame, funding,', 'if !attempt.is_claimed()',
    'drop(rq)', 'drop(attempt)', 'attempt.into_claimed().unwrap()',
    'drop(rq)', 'drop(activation)', 'drop(execution)'],
    'direct refusal outside admission and local refusal outside rq before original loan Drop')
if 'dispatch_still_eligible' in claim + runtime:
    raise SystemExit('joint funding claim regained check-then-CAS admission')
for source_path in Path('kernel/ps/src').rglob('*.rs'):
    source = source_path.read_text()
    for deleted in ('bind_ipc_priority_to_process_worker', 'release_ipc_priorities_for_process',
                    'set_next_synchronous_pick_hint'):
        if deleted in source:
            raise SystemExit(f'deleted process donation authority regained: {source_path}: {deleted}')
cold_adapter = body(Path('kernel/ps/src/multitask/current.rs').read_text(), 'commit_ipc_call_handoff')
ordered(cold_adapter, ['preemption_disabled()', 'held_spin_lock_depth()',
    'DispatchPreemption::enter()', 'scope.mask_irqs()',
    'ipc_publication::call_before_arm(', '&scope, endpoint, reply, donor_task_id, receiver_task_id'],
    'cold original masked CPU, no raw/deferred catalog compatibility grant')
if 'scheduler_mut(' in cold_adapter or 'scheduler_ref(' in cold_adapter:
    raise SystemExit('cold IPC producer regained global authority')
if 'fn commit_ipc_call_handoff(' in Path('kernel/ps/src/multitask/scheduler.rs').read_text():
    raise SystemExit('legacy fused IPC transaction regained')
ordered(body(producer, 'call_before_arm_observing'), ['!scoped(scope)',
    'CurrentWait::capture()', 'sender.id() != donor_id',
    'runqueue::wait::armed(sender.slot())', 'participant(sender.id())',
    'participant(receiver_id)', 'Binding::from_original', 'receive_call(receiver, endpoint,',
    'donation_publication::bind_original(scope, reply,', 'binding.original_pin()',
    'Publication::begin(scope, target)', 'sync_handoff::publish_original(target,',
    'last_effect.take().unwrap()()',
    '!woke && !inherited && donation_ledger::reply_already_dispatched(sender.original_pin(), reply)',
    'if let Some(last_effect) = last_effect.take()',
    'drop(result)', 'drop(loans)', 'drop(donor)', 'drop(sender)'],
    'same cold sender/funder/receiver through typed admission and final FIFO')
if 'sender.arm(' in body(producer, 'call_before_arm_observing') or 'sender.commit(' in body(producer, 'call_before_arm_observing'):
    raise SystemExit('cold producer armed/committed sender before wait_for_reply')
ordered(body((base / 'stack_resources/context.rs').read_text(), 'retained_scheduling_context'), [
    'BindingPin', 'RESOURCES.get(pin.slot())?.with', 'resources.start?.id != pin.id()',
    'resources.context?', '!context.scheduling_context.is_bound_to(pin.id())',
    'Some(context.scheduling_context.identity())'],
    'one original participant resource leaf qualifies start and context without scalar lifetime replacement')
ordered(body((base / 'donation_publication.rs').read_text(), 'from_original'), [
    'pin_donation(root_slot, root_id)', 'retained_scheduling_context(',
    'root.as_ref().unwrap_or(&donor)', 'donor,', '_root: root,', 'context,'],
    'exact original donor or root loan survives joint resource observation and publication')
ordered(body((base / 'donation_publication.rs').read_text(), 'bind_original'), [
    'Loans::from_original(donor, basis)', 'ReceiveLoans { donor: &donor, receiver }',
    'if reserved && inherit', 'bind_received_owned(&loans, reply, IpcReply)',
    'else if reserved', 'bind_owned(&loans, reply, IpcReply)'],
    'actual received request and sender wake candidate use distinct original funding effects')
received_bind = body((base / 'donation_ledger.rs').read_text(), 'bind_received_owned')
ordered(received_bind, ['loans.donor.basis()', 'DONATION_LEDGER.lock()',
    '!basis_live(&ledger, basis)', 'bind_basis_locked(', '!= Ok(Some(*basis))',
    'previous.donation.custody_active', 'loans.receiver.slot()', 'loans.receiver.id()',
    'upsert_locked(', 'basis.root_id, basis.root_slot',
    'previous.donation.priority_donated, Some(*basis)', 'bind_reserved_locked('],
    'actual consumer receives only the same original root source and trusted priority')
if 'funding_basis(' in received_bind or 'upsert_owned(' in received_bind:
    raise SystemExit('actual received request cannot replace its original reserved funding')
ordered(body((base / 'donation_ledger.rs').read_text(), 'reply_already_dispatched'), [
    'reply == 0', 'DONATION_LEDGER.lock()', 'find_reply(reply, DonationNamespace::IpcReply)',
    'entry.donation.donor_task_id == donor.id()', 'entry.donation.custody_active',
    'entry.receiver_slot.is_some()', 'IpcDonationTarget::BoundWorker(_)',
    'basis.donor_slot == donor.slot()', 'basis.donor_id == donor.id()',
    'current_basis_live(&ledger, basis)'],
    'already dispatched is only an exact original donor-bound live reply observation')
ordered(body(Path('services/initd/src/main.rs').read_text(), 'libc_syscall_status'), [
    'status == -1', '-i64::from(last_errno())', 'else { status }'],
    'libc syscall -1 is normalized from TLS errno, not misreported as EPERM')
wake = (base / 'wake_publication.rs').read_text()
ordered(body(wake, 'receive_call'), ['retained_wake_admitted(binding,',
    'REASON_ENDPOINT_RECEIVE, endpoint', 'before_wake, on_wake, true'],
    'typed cold receive uses sole wake engine and deferred terminal drain')
ordered(body(wake, 'retained_wake_admitted'), ['TypedWake::capture(&binding, kind, id)',
    '!before_wake(&binding)', 'TaskExecutionOwner::Current(', 'frame_readers::acquire(&pin, cpu)',
    '!defer_terminal &&', 'terminal_quarantine::publish(stop)',
    'clear_wait(slot, typed.as_ref())', 'let outcome = publish(slot, target, cpu)',
    'on_wake(&binding)', 'drop(reader)', 'drop(pin)', 'drop(typed)', 'Ok(binding)'],
    'original receive pending phase encloses billing before wake and original reader through FIFO')
enqueue = body(Path('kernel/compat/src/user/syscall/linux/ipc_ops.rs').read_text(),
    'enqueue_call_and_wake_with_handles_for_task')
ordered(enqueue, ['preemption_disabled()', 'held_spin_lock_depth()',
    'ipc_call_admission::reserve(task_id)?', 'enqueue_endpoint_call_with_handles_priority_and_custody(',
    'multitask::commit_ipc_call_handoff(', 'endpoint.raw(), reply.raw(), task_id, receiver_task_id',
    'ipc_call_admission::refused_wake_can_keep_delivered_reply(', 'reply, task_id, donation_admitted',
    'cancel_reply_wait(reply, task_id, ReplyCancelReason::SchedulingCustody)'],
    'actual IPC caller prepublication raw refusal, independent receipt proof or exact bounded cancellation')
ordered(body(Path('kernel/compat/src/user/syscall/linux/ipc_call_admission.rs').read_text(),
    'refused_wake_can_keep_delivered_reply'), [
    'endpoint_reply_custody_returned(reply, caller)', '|| (donation_admitted',
    'endpoint::received_request_retains_custody(reply, caller)'],
    'failed wake keeps only an independently completed reply or an admitted actually received request')
ordered(body(Path('kernel/ipc-runtime/src/ipc/endpoint_receive.rs').read_text(),
    'received_request_retains_custody'), ['REPLIES.with(reply.raw()', 'object.message_id',
    'ENDPOINT_MESSAGES.with(message_id', 'message.reply_id != reply.raw()',
    'message.caller_task_id != caller', '!message.published', '!message.request.is_empty()',
    'REPLIES.with(reply.raw()', 'object.message_id == message_id', '!object.used', '!object.consumed',
    'object.fast_frame.is_none()', 'custody.caller_task_id() == caller'],
    'receipt is a read-only exact Message-to-Reply observation, never an own funding bind or stale caller')
terminal = (base / 'terminal_publication.rs').read_text()
ordered(body(terminal, 'guarded'), ['terminal::Turn::begin(handle, (slot, id))',
    'turn.mark_stopped()', 'donation_ledger::release_task(id)',
    'census.final_donation_members()', 'donation_ledger::release_task(member_id)',
    'owner.publish(cleanup, effects)', 'turn.finish(publication)'],
    'original process membership owns last cancellation before final consumers, not PID catalog scan')
# Initial CPU production is an independently owned whole family, not a copy of
# reset behind a new name. Shared hidden descriptors use the sole Ticket engine;
# Running and hardware permissions remain separate original image/turn loans.
bootstrap = (base / 'bootstrap.rs').read_text()
initial = (base / 'bootstrap/initial_tasks.rs').read_text()
kernel_birth = (base / 'kernel_creation.rs').read_text()
if re.search(r'Scheduler::|scheduler_(?:mut|ref)\(|scheduler: Scheduler', bootstrap + initial):
    raise SystemExit('initial production regained legacy Scheduler authority')
if (base / 'architecture.rs').exists():
    raise SystemExit('legacy bootstrap architecture still exists')
for path, names in [(base.parent / 'scheduler.rs', ['reset']),
        (base / 'smp.rs', ['initialize_secondary_idle', 'prepare_secondary_idle_execution'])]:
    source = path.read_text()
    for name in names:
        if re.search(r'\bfn\s+' + name + r'\s*\(', source):
            raise SystemExit('legacy initial execution family remains: ' + name)
ordered(body(kernel_birth, 'commit_image'), ['ticket.is_reserved_current()',
    'ticket.begin_commit()', 'ticket.install_context(', 'ticket.publish_start(',
    'stack_resources::publish_binding(', 'frame_publication::publish(',
    'task_directory::record(', 'current_identity::publish_metadata(',
    'affinity_payload::initialize_reserved(&ticket, affinity, affinity)',
    'runqueue::admit_blocked(slot)', 'bind_local_owner_generation(',
    'ticket.retain_publication(view)', 'ticket.finish_commit()', 'backing'],
    'sole descriptor physical backing overlaps original Blocked ACK')
irq_leaf = Path('kernel/ps/src/multitask/irq_leaf.rs').read_text()
ordered(irq_leaf, ["pub(super) struct Guard<'a, T, const CLASS: u8>",
    "inner: TrackedSpinGuard<'a, T, CLASS>", '_mask: Mask', 'struct Mask',
    'PhantomData<*mut ()>'], 'raw guard drops before original IRQ mask, non-migratable lifetime')
ordered(body(irq_leaf, 'enter'), ['let restore = enabled()', 'disable()', 'Self { restore'],
    'IRQ mask before raw acquisition without another dispatch unit')
ordered(body(irq_leaf, 'lock'), ['let mask = Mask::enter()', 'self.inner.lock()',
    'Guard { inner, _mask: mask }'], 'unchanged ranked raw acquisition retains mask')
ordered(body(irq_leaf, 'try_lock'), ['let mask = Mask::enter()', 'self.inner.try_lock()?',
    'Some(Guard { inner, _mask: mask })'], 'failed try restores only original mask')
for leaf_path in ['retirement_cleanup.rs', 'thread_creation/work.rs', 'scheduling_runtime.rs',
        'donation_ledger.rs', 'runqueue.rs', 'sync_handoff.rs', 'dispatch_policy.rs',
        'runqueue/affinity_payload.rs', 'runqueue/wait/epoch.rs', 'runtime_profile/owner.rs',
        'linux_thread_state.rs', 'execution_interval.rs']:
    shared_leaf = (base / leaf_path).read_text()
    if 'irq_leaf::' not in shared_leaf or re.search(r'\\bTrackedSpin(?:Lock|Guard)\\b', shared_leaf):
        raise SystemExit('IRQ/cold shared scheduler leaf lost original IRQ mask: ' + leaf_path)
placement_cell = body(Path('kernel/ps/src/multitask/process_placement.rs').read_text(), 'with')
ordered(placement_cell, ['let protected = || f(&mut self.0.lock())',
    '#[cfg(rustos_boot_image)]', 'interrupts::without_interrupts(protected)',
    '#[cfg(not(rustos_boot_image))]', 'protected()'],
    'same bounded ProcessPlacement leaf under original CPU custody, only privileged mask differs')
if 'DispatchPreemption::enter' in placement_cell:
    raise SystemExit('ProcessPlacement leaf reacquires an exclusive dispatch unit')
domain_leaf = body((base / 'domain_admission.rs').read_text(), 'with')
ordered(domain_leaf, ['let protected = || {', 'let _guard = REGISTRY.lock()',
    'f(&Turn(PhantomData))', '#[cfg(rustos_boot_image)]',
    'interrupts::without_interrupts(protected)', '#[cfg(not(rustos_boot_image))]',
    'protected()'], 'same bounded domain leaf within original constructor CPU turn')
if 'DispatchPreemption::enter' in domain_leaf:
    raise SystemExit('domain registry reacquires an exclusive dispatch unit')
reserved_affinity = (base / 'runqueue/affinity_payload.rs').read_text()
ordered(body(reserved_affinity, 'initialize_reserved'), [
    'ticket: &super::super::stack_resources::admission::Ticket', 'ticket.is_committing()',
    'RunOwnerState::Dormant', 'current_identity::with_exact_publication(slot, ticket.task_id()',
    'affinity.binding.lock()', 'binding.is_some()', '*binding = Some(ticket.task_id())',
    'store(affinity, task_mask, process_mask, false)'],
    'same constructor Ticket and exact published UID before initial affinity, no fixture exemption')
for producer_path in ['kernel_creation.rs', 'root_creation.rs', 'thread_creation.rs']:
    if 'affinity_payload::initialize_affinity(' in (base / producer_path).read_text():
        raise SystemExit('real constructor regained scalar fixture affinity: ' + producer_path)
ordered(Path('formal/lifecycle-execution-claim/LifecycleExecutionClaim.tla').read_text(), [
    'FBirthIdentity ==', '!.birthIdentity = 3', 'FBirthAffinity ==', 'f.birthIdentity = 3',
    '!.birthAffinity = 3', 'FBirthPublish ==', 'f.birthIdentity = 3 /\\ f.birthAffinity = 3'],
    'same joint constructor publishes original UID then affinity before Blocked ACK')
ordered(body(initial, 'commit_kernel'), ['self.check(EFFECTS)',
    '(ticket.slot(), ticket.task_id()), (self.slot(), self.id())',
    'self.cpu() != 0 || rsp != 0', 'self.cpu() == 0 || rsp == 0',
    'kernel_creation::commit_image(', 'runqueue::admit_initial(self)',
    'execution_interval::begin_bootstrap(self, now)', 'self.check(EFFECTS)', 'backing'],
    'original initial image joins descriptor Running interval and backing')
initial_admission = (base / 'runqueue/admission.rs').read_text()
ordered(body(initial_admission, 'admit_initial'), ['image.assert_materializing()',
    'let slot = image.slot()', 'let cpu = image.cpu()', 'Some(image.id())',
    'admission::is_reserved(slot)', 'RunOwnerState::Blocked', '!wait::armed(slot)',
    'owner.next(RunOwnerState::Running, Some(cpu))',
    'compare_exchange(owner, running)', 'bind_local_owner_generation(slot, running.generation)'],
    'original initial image alone admits exact CPU Running after hidden descriptor ACK')
ordered(body(initial, 'initialize_owners'), ['image.assert_effects()',
    'stack_resources::assert_boot_vacant()', 'runqueue::reset_before_publication()',
    'dispatch_policy::reset()', 'sync_handoff::reset_all_cpus()'],
    'true empty initial prefix refuses instead of teardown')
ordered(body(initial, 'root'), ['image.assert_effects()', 'root_slots::reserve(',
    'execution_image::reserved_kernel_entry(&ticket', 'let backing = image.commit_kernel(',
    'image.publish_current()', 'save_simd_state(image.slot())', 'drop(backing)'],
    'born BSP physical frame and producer overlap both Current banks and SIMD')
ordered(body(initial, 'idle'), ['image.assert_effects()', 'image.ap_stack()',
    'admission::reserve(', 'core::ptr::write(', 'let backing = image.commit_kernel(',
    'image.publish_current()', 'drop(backing)'], 'exact parked AP stack and banks')
if re.search(r'reserved_kernel_entry|saved_rsp\(|validate_published_saved_context', body(initial, 'idle')):
    raise SystemExit('AP initial production regained synthetic frame/RSP authority')
ordered(body(initial, 'validate_root_frame'), ['turn.original_current()',
    'observed_frame_token(slot, cpu, true)', 'pin_for_identity(token, id)',
    'frame_readers::acquire(&pin, cpu)', 'validate_published_saved_context('],
    'original root Current pin and reader precede unsafe saved frame access')
ordered(body(bootstrap, 'prepare_and_publish'), ['let hardware = self.key.hardware()',
    'with_bootstrap_turn(', 'initial_tasks::validate_root_frame(turn)',
    'InitialArchitecture::capture(&hardware, turn)', '.install()', '.seal()',
    'publish_scheduler_initialized(&ready)'], 'original BSP image/turn through hardware before Ready')
ordered(body(Path('kernel/ps/src/multitask/cpu_local.rs').read_text(), 'with_bootstrap_turn'),
    ['scope.assert_current(cpu)', 'dispatch_turn::begin(preemption, slot)',
     'turn.mask_irqs()', 'install(&turn)', 'scope.assert_current(cpu)',
     'turn.publish_return(slot, idle).after_directory_unlock()'],
    'same original masked bootstrap turn through architecture and logical publication')
# Function names repeat across architecture impls, so constrain to this owner.
initial_arch = architecture[architecture.index("impl<'a> InitialArchitecture<'a>"):architecture.index('pub(in crate::multitask) struct CurrentArchitecture')]
ordered(body(initial_arch, 'capture'), ['turn.original_current()', 'image.assert_current(cpu)',
    'original_initial_is_active(turn)', 'ArchitectureState::original(turn)',
    'state.process', 'Some(Self { image, turn, state })'], 'borrowed initial image and active original interval')
ordered(body(initial_arch, 'install'), ['self.turn.original_current()',
    'self.image.assert_current(cpu)', 'original_initial_is_active(self.turn)',
    'load_address_space_phys(', 'set_privilege_stack(', 'set_kernel_stack_top(',
    'install_segments(self.state, false)'], 'original initial turn retains both banks through last register')
# Entire Current/target terminal entry family must use original parked units,
# not a catalog or fresh MM/UID/funding admission after terminal close.
current_terminal = (base / 'current_terminal.rs').read_text()
current_api = (base.parent / 'current.rs').read_text()
legacy = (base.parent / 'scheduler.rs').read_text()
for name in ['retire_current_user_task_due_to_fault', 'exit_current_task',
        'terminate_user_task', 'terminate_user_process', 'exit_current_process']:
    if re.search(r'\bfn\s+' + name + r'\s*\(', legacy):
        raise SystemExit('legacy terminal entry family remains: ' + name)
for name, anchor in [('terminate_user_task', 'current_terminal::terminate(task_id)'),
        ('retire_current_user_task_due_to_fault', 'current_terminal::fault('),
        ('exit_current_task', 'current_terminal::exit()'),
        ('terminate_user_process', 'group_exit::terminate(process_id)'),
        ('exit_current_user_process', 'group_exit::exit_current()')]:
    entry = body(current_api, name)
    if anchor not in entry or 'scheduler_mut(' in entry or 'scheduler_ref(' in entry:
        raise SystemExit('terminal API did not transfer complete authority: ' + name)
fault_callback = body(current_api, 'retire_current_user_task_due_to_fault')
if 'complete_retirement_side_effects(' in fault_callback or 'Fence::' in fault_callback:
    raise SystemExit('exception terminal callback waits with IF clear and can suppress its cleanup executor TLB ACK')
compat_fault = body(Path('kernel/compat/src/user/syscall/mod.rs').read_text(),
    'retire_current_linux_task_due_to_fault')
if any(token in compat_fault for token in ['record_linux_process_fault_termination(',
        'current_user_process_thread_count(', 'cleanup_retired_task_runtime_state(',
        'complete_retirement_side_effects(']):
    raise SystemExit('compat exception callback performs external cleanup or reelects the final process member')
ordered(body(terminal, 'guarded'), ['user_fault_vector: match owner.reason()',
    'TaskRetireReason::UserFault { vector, .. } => Some(vector)', 'owner.begin_effects()',
    'owner.publish(cleanup, effects)'], 'original terminal fault sealed before effects and consumers')
lifecycle_cleanup = body(Path('kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs').read_text(),
    'service_retired_task_runtime_cleanup')
ordered(lifecycle_cleanup, ['multitask::next_retired_task_cleanup()',
    'retired_process_fault_vector(', 'cleanup.process_terminal(), cleanup.user_fault_vector()',
    'super::record_linux_process_fault_termination(cleanup.process_id(), vector)',
    'cleanup_retired_task_runtime_state(', 'multitask::complete_retired_task_cleanup(cleanup)'],
    'interruptible runtime consumer records exact original final fault before cleanup ACK')
original = current_terminal[current_terminal.index("impl<'scope, 'irq> OriginalCurrent"):current_terminal.index('pub(in crate::multitask) fn fault')]
ordered(body(original, 'borrow'), ['CurrentAllocation::borrow(scope)',
    'current_cpu_task_slot() != Some(slot)', 'CurrentPermit::borrow(scope, slot)',
    'RunOwnerState::Running', 'owner.state.is_terminal()',
    'CurrentInterval::borrow(scope, slot, allocation.id())', 'Some(Self'],
    'same original physical/MM bank, exact Current UID bank and unique interval')
ordered(body(original, 'stop'), ['self.check()', 'self.allocation.slot()',
    'self.allocation.id()', 'self.stopped()', 'terminal_quarantine::begin(',
    'Producer::waiting_for_original(slot, id)', 'terminal_publication::guarded(producer',
    'self.check()'], 'original Current retains all units through LAST terminal effect')
ordered(body(current_terminal, 'fault_in'), ['OriginalCurrent::borrow(scope)',
    'current.context().user_mode', 'current.stopped()', 'current.stop(TaskRetireReason::UserFault'],
    'fault classification never dereferences untrusted user RSP')
ordered(body(current_terminal, 'producer'), ['self.binding.slot()',
    'terminal::Turn::begin(handle, (slot, id))', 'authorization.last_desired()',
    'mark_process_exiting_for_handle(handle, pid)', 'drop(authorization)',
    'terminal_quarantine::begin(slot, Some(id)', 'drop(self)', 'producer'],
    'target original binding transfers to original terminal producer before drain')
for anchor in ['scheduler_mut(', 'scheduler_ref(', 'pin_published_funding(',
        'pin_for_identity(', 'current_local_frame_token(', 'saved_rsp(', 'mark_slot_ready(']:
    if anchor in current_terminal:
        raise SystemExit('Current terminal regained scalar/catalog/fresh admission: ' + anchor)
interval = (base / 'execution_interval.rs').read_text()
ordered(body(interval, 'consume'), ['assert!(!cell.current_borrowed', 'cell.active.as_ref()',
    'cell.active.take()', 'cell.settled = Some((slot, task_id))'],
    'same original interval excludes early settlement before take and once-only charge')
ordered(body(interval, 'install'), ['assert!(!cell.current_borrowed', 'cell.active',
    'cell.active = incoming.take()'], 'no replacement during original Current effects')
group_api = (base.parent / 'current/group_exit.rs').read_text()
ordered(body(group_api, 'request'), ['held_spin_lock_depth()', 'target_by_pid(pid)',
    'queued_for(handle)', 'PendingExit::begin(handle, member)',
    'mark_process_exiting_for_handle(handle, pid)?', 'pending.publish(pid, Reason::Terminated'],
    'group admission marks exact original process before durable worker publication')
# Whole Linux signal/job-control authority family, including native witnesses,
# is no longer a Scheduler operation. Original census and cohort share the
# existing transaction engine without borrowing affinity write authority.
linux_owner = (base / 'linux_thread_state.rs').read_text()
signals = (base / 'linux_thread_state/signals.rs').read_text()
for name in ['queue_linux_signal', 'queue_linux_process_sigchld',
        'queue_linux_signal_to_slot', 'stop_current_linux_process',
        'continue_linux_process', 'transfer_pending_process_sigchld']:
    if re.search(r'\bfn\s+' + name + r'\s*\(', linux_owner):
        raise SystemExit('legacy signal/job-control authority remains: ' + name)
for name, anchor in [('queue_linux_signal', 'signals::queue('),
        ('queue_linux_process_sigchld', 'signals::process_sigchld('),
        ('stop_current_linux_process', 'signals::stop_current(')]:
    entry = body(current_api, name)
    if anchor not in entry or 'scheduler_mut(' in entry or 'scheduler_ref(' in entry:
        raise SystemExit('signal API retained catalog authority: ' + name)
recipient_signal = signals[signals.index('impl Recipient'):signals.index('struct Group')]
ordered(body(recipient_signal, 'capture'), ['pin_donation(slot, id)', 'context::read(slot)',
    'task_start::read(slot)?.id != id', 'context.user_abi != Some(UserAbi::Linux)',
    'context.process_id != Some(pid)', 'Some(Self'], 'original signal recipient before metadata effects')
ordered(body(recipient_signal, 'queue'), ['check_scope(scope)', 'signal == 0',
    'linux_signal_bit(signal)', 'Group::capture(&self)', 'group.finish(scope, None',
    'LINUX_THREAD_STATES[slot].lock()', 'cell.owner_tid != Some(self.pin.id())',
    'state.pending_signals |= bit', 'raise_signal_pending(slot)',
    'state.signal_mask & bit == 0', 'Binding::from_original(self.pin)',
    'guarded_owned(binding, eligible_cpus)'], 'same original recipient spans group, Linux and final wake')
group_signal = signals[signals.index('impl Group'):]
ordered(body(group_signal, 'capture'), ['Change::begin(handle, target.member())',
    'change.process_pin()?', 'change.live_members()', 'Cohort::begin_job_control(',
    'context.process_handle != Some(handle)', 'Some(Self'], 'original complete process census and every exact member pin')
ordered(body(group_signal, 'finish'), ['self.change.begin_effects()',
    'self.cohort.begin_effects()', 'self.cohort.set_job_stopped(',
    'request_owner_reschedule(slot)', 'self.change.process_pin()',
    'note_job_control(pin, self.pid, stop_signal)', 'process_sigchld_in(scope, parent',
    'self.change.publish_job_control(&mut self.cohort)',
    'self.cohort.finish_published()', 'self.change.finish()'], 'last group/recipient effect before one dispatch admission release')
ordered(body(signals, 'stop_in'), ['OriginalCurrent::borrow(scope)',
    'current.member()', 'Recipient::capture(', 'Group::capture(&target)',
    'group.finish(scope', 'current.check()'], 'same original Current physical/MM/UID/interval through group stop')
placement = (base.parent / 'process_placement.rs').read_text()
ordered(body(placement, 'publish_metadata'), ['state.phase == 1', 'if let Some(mask)',
    'cohort.owns_affinity(', 'state.mask = mask', 'cohort.owns_job_control(',
    'cohort.publish_admission()', 'state.phase = 2'], 'sole transaction engine keeps job and affinity rights distinct')
cohort_source = (base.parent / 'current_identity/cohort.rs').read_text()
ordered(body(cohort_source, 'set_job_stopped'), ['self.owns_job_control(slot, id)',
    'writer.bound_task_id != Some(id)', 'writer.lifecycle.retired',
    'writer.lifecycle.job_stopped = stopped', 'commit_writer(slot, writer)'],
    'exact original job cohort never resurrects closed bindings')
compat_signal = Path('kernel/compat/src/user/syscall/linux/support.rs').read_text()
stop_case = compat_signal[compat_signal.index('        PROCD_SELECT_SIGNAL_STOP => {'):compat_signal.index('        PROCD_SELECT_SIGNAL_HANDLER => {')]
ordered(stop_case, ['let Some(changed) = multitask::stop_current_linux_process(signal)',
    'return false', 'clear_current_pending_signal(signal, response.reserved0)',
    'if changed', 'multitask::yield_now()'], 'untouched busy Current stop never consumes the pending cause')
broker_signal = Path('kernel/compat/src/user/syscall/linux/proc_broker_ops.rs').read_text()
if 'Err(()) => linux_errno(LINUX_EAGAIN)' not in broker_signal:
    raise SystemExit('signal group contention falsely reports missing task instead of retry')
observation = (base / 'current_observation.rs').read_text()
for name in ('current_scheduling_context_runtime_snapshot', 'set_current_tls_fs_base',
             'mark_root_idle', 'set_current_alternate_kernel_stack',
             'restore_current_alternate_kernel_stack', 'user_log_ids_for_task',
             'linux_thread_snapshot_by_ids', 'is_user_task_alive'):
    if re.search(r'\bfn\s+' + name + r'\s*\(', (base.parent / 'scheduler.rs').read_text()):
        raise SystemExit('cold legacy observation authority remains: ' + name)
for source in (base.parent / 'mod.rs', Path('kernel/ps/src/api.rs')):
    if 'CurrentKernelStackScope' in source.read_text():
        raise SystemExit('unused scalar-only alternate-stack authority remains: ' + str(source))
ordered(body(observation, 'set_tls_in'), ['OriginalCurrent::borrow(scope)', 'current.context()',
    'current.member()', 'linux_thread_state::exact_state(slot, id)', 'state.fs_base == value',
    'runqueue::simd_tls::set_tls_fs_base(slot, value)', 'current.check()'], 'same original Current through accepted TLS cache')
ordered(body(observation, 'mark_root_idle_in'), ['OriginalCurrent::borrow(scope)',
    'current.member() != (super::ROOT_TASK_SLOT, 0)', 'current.context().user_mode',
    'runqueue::set_idle_slot(super::ROOT_TASK_SLOT, true)', 'current.check()'], 'original root Current through idle publication')
for query in (body(signals[:signals.index('struct Group')], 'resolve'),
              body((base / 'current_terminal.rs').read_text().split('impl TargetStop')[1], 'capture'),
              body(observation, 'capture')):
    if 'task_directory::candidates(id).find_map(' not in query:
        raise SystemExit('exact target lookup treats accelerator miss as authoritative absence')
ordered(body((base / 'task_directory.rs').read_text(), 'candidates'), [
    'let first = hint(task_id)', 'first.into_iter().chain((0..MAX_TASK)',
    'Some(*slot) != first'], 'bounded candidate fallback after missing or stale hint')
if 'Target::capture' in body(observation, 'alive_in'):
    raise SystemExit('transient metadata pin refusal falsely reports task death')
ordered(body(observation, 'alive_in'), ['task_directory::candidates(id)',
    'identity::observe_user_liveness(slot, id)'], 'exact scalar liveness, never an execution grant')
ordered(body(observation, 'capture'), ['task_directory::candidates(id)',
    'identity::execution::pin_donation(slot, id)', 'stack_resources::context::read(slot)',
    'task_start::read(slot)?.id != id'], 'original target metadata UID before context')
ordered(body((base / 'execution_interval.rs').read_text(), 'runtime_snapshot'), [
    'self.check()', 'with_interval(self.scope.cpu()', 'FundingPin::observation',
    '})?', 'FundingObservation { original: self, stamp, owner }',
    'scheduling_runtime::observe_current_funding(&observation)',
    'self.check()'], 'same original interval and funding; interval leaf ends before budget pair')
ordered(body((base / 'scheduling_runtime.rs').read_text(), 'observe_current_funding'), [
    'original.metadata()', 'cell(slot)?.lock()', 'context.active_intervals == 0',
    'domain_cell(domain_slot)?.lock()', 'domain.active_intervals == 0',
    'context.budget.as_ref()?.snapshot()', 'domain.bank(original.cpu())?.snapshot()'],
    'original paired read preserves Context before Domain order without raw interval leaf')
ordering = (base / 'ordering_publication.rs').read_text()
legacy_files = {name: (base / name).read_text() for name in ('handoffs.rs', 'ipc_donation.rs', 'activation.rs', 'thread_slots.rs',
    'execution_image.rs', 'smp.rs', 'affinity.rs', 'linux_thread_state.rs', 'task_wait.rs')}
legacy_files['scheduler.rs'] = (base.parent / 'scheduler.rs').read_text()
for source, names in (
    ('scheduler.rs', ('set_next_pick_hint', 'set_next_latency_pick_hint', 'set_next_process_pick_hint', 'wake_user_task',
        'pick_hint_candidate_slot', 'take_next_latency_pick_hint_ready_slot',
        'effective_slot_class', 'resolve_donation_slot', 'charge_effective_scheduling_context_runtime',
        'exec_current_user_process', 'exec_user_process_by_pid', 'exec_slot_admission_valid',
        'collect_live_process_sibling_slots', 'replace_slot_exec_context', 'first_allocatable_slot',
        'allocate_kernel_slot', 'allocate_user_slot', 'native_root_catalog',
        'allocate_user_slot_with_scheduling_context', 'allocate_user_process_state_slot',
        'allocate_stack_storage', 'reset_stack_storage', 'stack_storage_mut', 'stack_top',
        'wake_task', 'wake_task_slot')),
    ('task_wait.rs', ('arm_block_current_task', 'arm_block_current_task_on_endpoint',
        'arm_block_current_task_on_reply', 'arm_block_current_task_on_pager_fault',
        'arm_block_current_task_on_pager_service', 'arm_block_current_task_with_reason',
        'cancel_block_current_task', 'commit_block_current_task')),
    ('thread_slots.rs', ('reserve_user_thread_slot', 'thread_commit_affinity', 'commit_user_thread_slot',
        'cancel_user_thread_slot', 'slot_thread_reserved')),
    ('execution_image.rs', ('init_reserved_kernel_entry_context', 'commit_exec_user_image', 'commit_current_exec_image',
        'init_kernel_entry_context', 'init_user_task_context')),
    ('smp.rs', ('process_handle_for_thread', 'quiesce_captured_exec', 'cancel_exec_target_quiesce',
        'assert_exec_target_replacement_safe', 'context_is_migratable_from_source')),
    ('affinity.rs', ('exec_affinity_snapshot', 'assert_exec_affinity_preserved')),
    ('linux_thread_state.rs', ('find_linux_thread_slot',)),
    ('handoffs.rs', ('report_invalid_activation_context', 'activate_suspended_user_task',
        'activate_suspended_user_tasks_with_commit', 'activate_suspended_user_tasks',
        'preflight_suspended_user_tasks', 'assert_authority_commit_preserved_suspension',
        'publish_suspended_user_tasks', 'set_next_spawn_pick_hint', 'enqueue_synchronous_handoff_slot',
        'take_next_bootstrap_handoff_ready_slot', 'take_next_spawn_pick_hint_ready_slot',
        'take_next_spawn_pick_hint_ready_record', 'take_next_atomic_activation_handoff_ready_slot',
        'materialize_rejected_direct_record', 'synchronous_handoff_record_is_ready',
        'take_next_synchronous_pick_hint_ready_slot', 'take_next_synchronous_pick_hint_ready_record',
        'immediate_synchronous_handoff_ready_slot', 'record_synchronous_handoff',
        'synchronous_handoff_len_for_tests', 'set_synchronous_handoff_streak_for_tests',
        'handoff_identity_matches', 'published_fifo_candidate_ready', 'published_fifo_identity_matches')),
    ('ipc_donation.rs', ('eligible_process_worker_slot', 'apply_ipc_donation',
        'scheduling_context_owner_is_live', 'reserve_ipc_call_donation',
        'effective_scheduling_context_owner_slot', 'effective_scheduling_context_charge_token',
        'reserve_ipc_priority', 'reserve_ipc_priority_with_context',
        'cancel_ipc_priority_reservation', 'attach_reserved_ipc_priority',
        'bind_reserved_ipc_priority', 'inherit_ipc_priority', 'inherit_pager_fault_priority',
        'upsert_ipc_priority_donation', 'ipc_priority_donation_capacity_available',
        'release_ipc_priority', 'release_ipc_priorities_for_task', 'remove_ipc_priority_donation')),
    ('activation.rs', ('commit',))):
    for name in names:
        if re.search(r'\bfn\s+' + name + r'\s*(?:<|\()', legacy_files[source]):
            raise SystemExit('legacy ordering/activation authority remains: ' + source + '::' + name)
for obsolete in ('ipc_priority_donations:', 'ipc_priority_donation_len:',
                 'donation_donor_slot_hints:', 'MAX_IPC_DONATION_CHAIN_DEPTH', 'thread_slot_reserved:'):
    if obsolete in legacy_files['scheduler.rs']:
        raise SystemExit('legacy donation bank/recursive authority remains: ' + obsolete)
if 'impl Scheduler' in legacy_files['thread_slots.rs'] or 'UserThreadSlotReservation' in legacy_files['thread_slots.rs']:
    raise SystemExit('legacy thread reservation implementation/type remains')
owned_wait = (base / 'current_wait/tests/legacy_witnesses.rs').read_text()
ordered(body(owned_wait, 'race_and_typed_cancel'), ['set_saved_rsp(slot, 1)',
    'let wait = capture().unwrap()', 'wait.arm(BlockReason::Generic)',
    'wake_publication::wake_task(ID)', '!wait.commit()', 'wait.commit()',
    '!wait.arm(BlockReason::Generic)', '!wait.cancel()',
    'BlockReason::EndpointReceive(0xfeed)', 'wait.cancel()',
    'cpu_leases::owns_current(0, slot)', 'set_saved_rsp(slot, saved)'],
    'original Current wait survives raced/committed wake without consumed frame read')
owned_transition = (base / 'dispatcher/maintenance/tests/legacy_witnesses.rs').read_text()
ordered(body(owned_transition, 'transition_wake'), ['dispatch_claim::tests::fixture(false',
    'test_blocked_return(', 'install_test_transition_owner(', 'wake_publication::guarded(',
    'RemoteQueued', 'wake_publication::guarded(', 'owner(slot), published',
    'transition.commit_assembly()', 'drain_remote_wakes(1)', 'transfer(turn, slot, 1, 0)',
    'frame.prepare().unwrap().claim()', 'banks::owns_execution(0, slot)',
    'FrameCapture::Stale', 'finish_modeled_preparation('],
    'born physical mailbox/drain/claim join; earlier Transition and RSP ACK explicitly modeled')
owned_thread = (base / 'thread_creation/tests/legacy_witnesses.rs').read_text()
ordered(body(owned_thread, 'reserved_exclusion'), ['admission::reserve(FIRST_DYNAMIC_TASK_SLOT',
    'root_slots::reserve(pending.slot()', 'pending.finish_cancel()', 'let r = reserve_actual()',
    'root_slots::reserve(slot', 'r.ticket().is_reserved_current()', 'cancel(r)',
    'replacement.generation()'], 'actual pre-storage Ticket and complete thread/MM exclude process producer')
owned_census = (base / 'exec_transaction/quiescence/tests/legacy_witnesses.rs').read_text()
ordered(body(owned_census, 'pending_kernel'), ['ExecMembership::begin(&intent)',
    'test_with_pending_kernel(target', 'check(&intent, &census), None',
    'kernel_pending(target)', 'after.saved_rsp, context.saved_rsp', 'cancel(intent, census)'],
    'actual exec refuses unconsumed kernel payload without target bytes or sibling effects')
ledger = (base / 'donation_ledger.rs').read_text()
for name in ('reserve', 'bind_reserved', 'upsert'):
    if re.search(r'\bfn\s+' + name + r'\s*\(', ledger):
        raise SystemExit('unowned scalar ledger insertion authority remains: ' + name)
# Topology never enters dispatch's stack: resolve ONE exact original source in
# the finite ledger, not a recursive clone of the removed catalog algorithm.
for name in ('basis_locked', 'source_live', 'inherited_system'):
    entry = body(ledger, name)
    if re.search(r'\b' + name + r'\s*\(', entry[entry.index('{') + 1:]):
        raise SystemExit('recursive donation owner: ' + name)
ordered(body(ledger, 'basis_locked'), ['borrowed_context_charge_token(slot)',
    'ledger.find_reply(reply, namespace)?', 'source_live(ledger, source)',
    'root_id: source.root_id'], 'bounded root-compressed original source, not four-hop truncation')
owned_ipc = (base / 'donation_publication/tests/legacy_witnesses.rs').read_text()
ordered(body(owned_ipc, 'nested_interval'), ['donation_roots(ID,',
    'reserve_call_with(scope, ID)', 'bind_with(scope, 11,', 'test_open_captured',
    'release_reply(11, IpcReply)', 'original.charge(100, 4500)',
    'ConsumedInterval::AlreadySettled', 'native.charge(200, 4500)'],
    'old root billing witness retains original interval across reply withdrawal')
for name, anchor in (
    ('set_next_pick_hint', 'ordering_publication::target('),
    ('set_next_spawn_pick_hint', 'ordering_publication::spawn('),
    ('set_next_latency_pick_hint', 'ordering_publication::latency('),
    ('set_next_process_pick_hint', 'ordering_publication::process('),
    ('wake_user_task', 'wake_publication::wake_user('),
    ('activate_suspended_user_tasks_with_commit', 'activation::activate(')):
    entry = body(current_api, name)
    if anchor not in entry or 'scheduler_mut(' in entry or 'scheduler_ref(' in entry:
        raise SystemExit('actual ordering/wake/activation facade retains catalog: ' + name)
if any(token in ordering for token in ('scheduler_mut(', 'scheduler_ref(', 'SCHEDULER', 'unsafe {')):
    raise SystemExit('ordering publication reintroduced guarded or unsafe authority')
ordered(body(ordering, 'capture'), ['task_directory::candidates(id)',
    'Self::from_binding(wake_publication::Binding::capture(slot, id)?)'], 'original pin passed intact to ordering qualification')
# from_binding receives the non-Copy pin: no copied winning slot may replace it.
ordered(body(ordering, 'from_binding'), ['binding.slot()', 'runqueue::owner(slot)',
    '!owner.runnable', 'runqueue::wait::blocked(slot)', 'ipc_publication::eligible(&binding, cpu)',
    'Some(Self { binding, cpu })'], 'qualified original runnable ordering target')
ordered(body(ordering, 'floor'), ['current.member()', 'target.binding.slot()',
    'donor == receiver || runqueue::is_idle_slot(donor)', 'observed_class(receiver)',
    'ready_floor(target.cpu', 'lower_vruntime_ceiling(receiver, caller.min(band))',
    'current.check()'], 'same Current and receiver through nonauthoritative floor')
for name in ('target_in', 'spawn_in', 'process_in'):
    entry = body(ordering, name)
    ordered(entry, ['OriginalCurrent::borrow(scope)', 'Publication::begin(scope, target.cpu)',
        'target.ready()', 'floor(current.as_ref(), &target)'], 'publication arbitration before ' + name)
ordered(body(ordering, 'latency_in'), ['Target::capture(id)',
    'Publication::begin(scope, target.cpu)', '!target.ready() || !local_dispatch::stable_user_hint_class(slot)',
    'previous.slot == slot', 'latency_pick_hint_len == MAX_LATENCY_HANDOFF_HINTS'], 'bounded qualified User FIFO without a floor or new funding')
ordered(body(ordering, 'process_in'), ['Binding::capture(slot, id)',
    'target.binding.context().process_id', '.then_some(target)', '.min_by_key(',
    'Publication::begin(scope, target.cpu)', 'SyncHandoffRecord::new(target.binding.slot(), target.binding.id())',
    'sync_handoff::enqueue(target.cpu, record)'], 'winning original process target through FIFO and notification')
ordered(body(wake, 'wake_user_in'), ['ipc_publication::scoped(scope)',
    'task_directory::candidates(id)', 'Binding::capture(slot, id)',
    '!binding.context().user_mode', 'guarded_owned(binding, eligible)'], 'same original user-qualified wake binding')
ordered(body(dispatch, 'hint'), ['next_pick_hint.take()', 'current_identity::read_dispatch(hint.slot)',
    'p.task_id == Some(hint.task_id)', 'local_dispatch::user_hint_blocked_by_system(',
    'local_candidate(slot, cpu, Urgency::Hint)'], 'generic hint UID before candidate admission')
# The full cross-MM/reply/kernel return witness reaches both of these paths.
# Blocked outgoing custody must bind the same CAS generation before its later
# reply; requiring a process activation pin for a pure kernel image is invalid.
returned = body((base / 'runqueue/return_publication.rs').read_text(), 'publish_return_inner')
ordered(returned, ['OWNER_WORDS[slot].compare_exchange(observed, next).is_ok()',
    'frame_publication::bind_local_owner_generation(slot, next.generation)'],
    'successful outgoing CAS binds Blocked and Local frame generation')
if 'if local { frame_publication::bind_local_owner_generation' in ' '.join(returned.split()):
    raise SystemExit('Blocked return lost exact frame generation binding')
ordered(body(dispatch, 'complete'), ['stack_resources::context::retained_architecture(',
    'if incoming.process', 'turn.claimed_mm_root(next, receipt.id)',
    '} else { incoming.root }', 'assert_eq!(root, incoming.root'],
    'original process activation versus exact pure-kernel image')
bitmap = body((base / 'runqueue.rs').read_text(), 'local_runnable_snapshot')
if 'cfg(' in bitmap or '[u64::MAX;' in bitmap:
    raise SystemExit('native dispatch replaced actual published runnable bitmap')
ordered(bitmap, ['core::array::from_fn', 'published[word].load(Ordering::Acquire)'],
    'one actual runnable bitmap in native and production dispatch')
# Frozen shared-MM census reaches the same original executor's external work,
# real process detach and exact row ACK in native and production paths.
cleanup = (base / 'retirement_cleanup.rs').read_text()
ordered(body(cleanup, 'complete_with'), ['self.executor.assert_current()',
    'external(self.effects)', 'self.executor.assert_current()',
    'DispatchPreemption::enter()', 'preemption.mask_irqs()',
    'CELLS[self.slot].complete_kernel(self.generation, self.effects)',
    'mark_reap_pending()'], 'original kernel executor through last ACK')
ordered(body(cleanup, 'complete_modeled_external'), ['self.complete_with(',
    'effects.complete_external_with(external)'], 'native external input shares actual claim engine')
reclaim = (base / 'reclaim.rs').read_text()
ordered(body(reclaim, 'complete_external_with'), ['!nucleus_core::util::lockdep::preemption_disabled()',
    'external(&self)', 'self.detach_process_handle', 'process_table::detach_task(process_handle)'],
    'same actual process detach after external work before row ACK')
identity = Path('kernel/nucleus-core/src/util/lockdep/cpu_identity.rs').read_text()
ordered(identity, ['#[cfg(all(not(rustos_boot_image), feature = "native-cpu-model"))]',
    'mod native', 'std::thread_local!', 'struct Restore', 'impl Drop for Restore',
    'CPU.with(|cpu| cpu.set(self.0))', 'assert!(cpu < super::super::MAX_TRACKED_CPUS',
    'Restore(CPU.with(|old| old.replace(cpu)))'], 'bounded thread-local native architectural input only')
foreign = (base / 'irq_entry_tests/foreign_shared.rs').read_text()
ordered(foreign, ['with_native_cpu(1,', 'wake_publication::wake(',
    'entry.dispatch(', 'ExecMembership::begin(&intent)', 'census_pending(',
    'commit_context_switch()', 'census_pending(', 'entry.dispatch(', 'census_pending(',
    'commit_context_switch()', 'exec_transaction::quiescence::quiesce(',
    'claim_next_kernel(&executor)', 'complete_modeled_external(', 'kernel_pending(sibling)',
    'process_table::authorize_exec(reservation)', 'exec_transaction::with_census_image(',
    'exec_transaction::publication::commit(', 'process_table::finalize_exec_state(',
    'census.finish(&finalized)', 'old.retire_execution_state()',
    'complete_original_retired(sibling', 'terminal_publication::guarded(', 'entry.dispatch(',
    'commit_context_switch()', 'complete_original_retired(source'], 'foreign original census through actual exec and terminal returns')
if 'sync_handoff_states' in legacy_files['scheduler.rs'] or 'per_scheduler_locks' in (base / 'sync_handoff.rs').read_text():
    raise SystemExit('isolated catalog FIFO storage still substitutes for actual owners')
fifo = (base / 'dispatcher/fifo_witnesses.rs').read_text()
ordered(body(fifo, 'first_turn_fifo'), ['dispatch_claim::tests::fixture(false',
    'enqueue(slot, ID + 1, atomic)', 'Decision::Empty', 'enqueue(slot, ID, atomic)',
    'exact duplicate preserves FIFO position', 'choose(turn, 0, 121',
    'Decision::Claimed(urgency)', 'execution_interval::original_claim_is_active(turn)',
    'sync_handoff::model_len(0)', 'finish_modeled_preparation(slot, 121)'],
    'historical FIFO witness reaches SAME original born frame/funding claim before lower queues')
ordered(body(fifo, 'incomplete_first_turn'), ['Decision::Incomplete', 'enqueue(unknown,',
    'first_turn(turn', 'Decision::Incomplete', 'assert!(!empty(atomic))', 'turn.claimed_return().is_none()'],
    'unknown original FIFO cannot be consumed or admit a later arm')
ordered(body(fifo, 'synchronous_fifo'), ['policy_witnesses::family(', 'match synchronous(0)',
    'sync_handoff::model_len(0), 2', 'Some((first,', 'Some((second,',
    'sync_handoff::record_dispatch(0, true, 1)', 'MAX_SYNC_HANDOFF_CHAIN_TICKS',
    'sync_handoff::model_len(0), 2', 'sync_handoff::record_dispatch(0, false',
    'sync_handoff::remove_slot_all_cpus(second)', 'id(first) + 99', 'runqueue::owner(first), owner'],
    'actual shared FIFO preserves exact identities, dedup, chain clock and refusal custody')
# Selection/class/budget resource join is now IN the SAME foreign protocol as
# Current/Transition, original settlement, shared-MM exec census, both cleanup
# ACKs and fresh constructor/terminal incarnation; not a fourth disjoint mode.
foreign_model = Path('formal/lifecycle-execution-claim/LifecycleExecutionClaim.tla').read_text()
ordered(foreign_model, ['FSelect ==', '!.selectPin = TRUE', 'FObserve ==',
    '!.reader = FALSE', 'FFund ==', '!.fundingGrant = TRUE', '!.originalContext = 2',
    'FAdmission ==', 'f.classEpoch = f.observedClass', 'f.queueGeneration = f.observedGeneration',
    'f.contextCredit /\\ f.domainCredit', 'FJoin ==', '!.claimLeaf = TRUE',
    'FClaim ==', 'f.selection = "Locked"', 'f.claimLeaf /\\ f.admitted',
    '!.peerPin = TRUE', '!.peerActive = TRUE', '!.selectPin = FALSE', '!.fundingGrant = FALSE',
    'FRefuse ==', 'FClassPublish ==', 'FContextDebit ==', 'FDomainDebit ==',
    'FQueueRepublish ==', 'FStopBeforeClaim ==', 'FPostClaimDonation ==', 'FEffect ==',
    'FLeave ==', '!.chargedContext = f.originalContext', 'FDetach ==',
    'FDetachUnclaimed ==', '~f.selectPin /\\ ~f.reader /\\ ~f.fundingGrant',
    'FExec ==', 'FFinalize ==', 'FSourceReset ==', 'FBirthReserve ==',
    'f.kernel0Active /\\ f.kernel1Active', '!.birthTicket = TRUE', 'FBirthPublish ==',
    'f.birthTicket /\\ f.birthNonce = 2', 'FRebornStop ==', 'FNewExternalAck ==',
    'FOldExternalAck ==', 'f.birthNonce = 1', 'FNewKernelAck ==', 'FRebornReset ==',
    'f.newExternal /\\ f.newKernel', 'ForeignNext ==', 'FSelect \\/ FObserve \\/ FFund \\/ FJoin \\/ FClaim',
    'FRebornStop \\/ FNewExternalAck'], 'one source-refining foreign selection-to-rebirth protocol')
ordered(foreign, ['entry.dispatch(', 'install(&mut entry, dispatch, fast)',
    'donation_publication::bind_with(&scope, 0x7f21', 'borrowed_context_charge_token(sibling), Some((source, 0x7f21))',
    'ExecMembership::begin(&intent)', 'census_pending(', 'commit_context_switch()',
    'OriginalCurrent::borrow(&scope)', 'original.runtime_snapshot()',
    '(snapshot.executing_task_id, snapshot.context_owner_task_id), (ID + 1, ID + 1)',
    'borrowed_context_charge_token(sibling), Some((source, 0x7f21))',
    'census_pending(', 'entry.dispatch(', 'commit_context_switch()', 'quiesce(',
    'borrowed_context_charge_token(sibling), None', 'claim_next_kernel(&executor)',
    'exec_transaction::with_census_image(', 'old.retire_execution_state()',
    'complete_original_retired(sibling', 'terminal_publication::guarded(', 'commit_context_switch()',
    'complete_original_retired(source', 'active_independent_root_with_scope(', 'assert_eq!(reborn, source)',
    'assert_ne!(reborn_handle, handle)', 'Binding::capture(reborn, ID).is_none()',
    'Binding::capture(reborn, ID + 2).is_some()', 'owns_current(0, 121)', 'owns_current(1, EXECUTOR)',
    'terminal_quarantine::begin(reborn', 'terminal_publication::guarded(stop',
    'drop(masked)', 'complete_original_retired(reborn, ID + 2, Some(old_cleanup))',
    'execution_interval::consume(cpu, slot, id)'],
    'actual original shared-MM foreign interval, metadata replacement and same-bank new incarnation')

ordered(body(foreign, 'complete_original_retired'), [
    'executor::capture_current()', 'kernel_pending(slot)', 'claim_next_kernel(&executor)',
    '.complete(|task|', 'wake_publication::wake_task(task)', '!retirement_cleanup::kernel_pending(slot)',
    'retirement_cleanup::claim_next()', 'cleanup.task_id(), id', 'if let Some(old) = replay',
    '!retirement_cleanup::complete(old)', 'retirement_cleanup::complete(cleanup)',
    'reaper::claim_next(&executor)', 'context::read(slot).is_none()', 'reclaim.complete()',
    'current_identity::read(slot).is_none()'], 'actual kernel effects, independent exact ABI ACK and real reaper through old and new incarnations')
if any(obsolete in foreign for obsolete in ['ack_kernel_fixture(', 'release_published(', 'release_after_kernel_ack(']):
    raise SystemExit('foreign source-refining join regained a fixture cleanup/ACK adapter')
ordered(foreign, ['with_peer_backing(true, false, policy,', 'state_with_root(0x8000)',
    'with_native_cpu(1,', '0x1000, 0x4000'], 'initial owned user MM is distinct from real kernel executor MM before birth')

# Whole deletion includes host backends: native success must not be supplied by
# a duplicate catalog, cfg-hidden Scheduler or a guard with a different spelling.
for source in Path('kernel/ps/src/multitask').rglob('*.rs'):
    text = source.read_text()
    if re.search(r'\b(?:struct\s+Scheduler\b|impl\s+Scheduler\b|fn\s+scheduler_(?:mut|ref)\b|static\s+SCHEDULER\s*:)', text):
        raise SystemExit('deleted legacy Scheduler authority returned: ' + str(source))
for obsolete in ['address_space_root.rs', 'task_bindings.rs', 'retirement_control.rs']:
    if (base / obsolete).exists():
        raise SystemExit('obsolete catalog adapter returned: ' + obsolete)
namespace = (base / 'tests/part_1.rs').read_text()
fixture = namespace[namespace.index('pub(super) struct SchedulerTestFixture'):namespace.index('fn boxed_scheduler')]
if re.search(r'\b(?:Scheduler|TaskContext|TaskStart|Deref|Box)\b', fixture):
    raise SystemExit('native namespace fixture regained catalog/backend authority')
cleanup = (base / 'tests/catalog_witnesses.rs').read_text()
ordered(body(cleanup, 'exact_cleanup'), ['exec_root_with_backing', 'state_with_root(0x9000)',
    'terminal_quarantine::begin', 'terminal_publication::guarded', 'capture_current()',
    'reaper::claim_next(&executor).is_none()', 'claim_next_kernel(&executor)',
    '!retirement_cleanup::kernel_pending(slot)', 'reaper::claim_next(&executor).is_none()',
    'retirement_cleanup::claim_next().unwrap()', 'retirement_cleanup::claim_next(), None',
    'reaper::claim_next(&executor).is_none()', '!retirement_cleanup::complete(foreign)',
    'retirement_cleanup::complete(cleanup)', 'reaper::claim_next(&executor).unwrap()',
    'thread_count_by_pid(PID), Some(1)', 'reclaim.complete()', 'thread_count_by_pid(PID), Some(0)'],
    'old cleanup witnesses retain original born MM and independently Pending/Claimed runtime ACK after real kernel ACK')
# Standalone identity witnesses share the real birth/resource namespace.
# Per-writer atomics cannot stop another fixture from rebinding its live slot.
# This guard excludes other TESTS, never concurrent actors inside a witness.
for path, names in [
    (Path('kernel/ps/src/multitask/current_identity.rs'), [
        'published_identity_round_trips_and_clears', 'dispatch_lifecycle_is_committed_with_identity',
        'kernel_task_publishes_no_user_binding', 'incomplete_user_or_odd_publication_requires_locked_log_lookup']),
    (Path('kernel/ps/src/multitask/current_identity/owner_tests.rs'), [
        'metadata_republication_cannot_erase_newer_lifecycle_intent',
        'absent_partial_metadata_preserves_pending_start_and_stop_intent',
        'stale_live_command_cannot_stop_or_resume_a_recycled_identity',
        'concurrent_metadata_and_gate_writers_do_not_lose_each_others_fields'])]:
    source = path.read_text()
    for name in names:
        ordered(body(source, name), ['execution_test_serial_guard()', 'let slot', 'clear(slot)'],
                'standalone original Identity fixture namespace: ' + name)
ordered((base / 'scheduling_runtime/namespace_tests.rs').read_text(),
    ['exec_root(97101, true', 'identity::read(slot)', 'scope.spawn(', 'TEST_GUARD.try_lock()',
     'Err(TryLockError::WouldBlock)', 'identity::read(slot), Some(before)',
     'TEST_GUARD.lock().unwrap_or_else'], 'deterministic actual birth/Identity namespace exclusion and release')
# HECS uses the SAME actual arrival checker and the existing authenticated
# private-workload/archive boundary, not a native-model or user-print PASS.
ordered(body(rsp_handoff, 'assert_arrived'), ['self.incoming.contains(rsp)',
    '!self.outgoing.contains(rsp)', 'execution::banks::owns_outgoing(cpu, self.from)',
    '#[cfg(not(test))]', 'HARDWARE_ARRIVALS[cpu].0.fetch_add(1, Ordering::Relaxed)'],
    'only actual machine callback observations increment HECS arrival evidence')
hecs_host = (base.parents[4] / 'tools/xtask/src/kvm/hecs.rs').read_text()
ordered(body(hecs_host, 'is_complete'), ['evidence(log, workers).is_ok()'],
    'actual HECS readiness uses the tested full evidence validator')
hecs_guest = (base.parents[4] / 'tools/xtask/src/kvm/guest.rs').read_text()
ordered(hecs_guest, ['let smp_ring3_qualification_ready',
    'smp_ring3_qualification_is_complete(&rustos_log, options.rustos_vcpus)',
    'hecs::is_complete(&rustos_log, options.rustos_vcpus)',
    '&& smp_ring3_qualification_ready'], 'actual KVM readiness requires both independent obligations')
hecs_archive = (base.parents[4] / 'tools/xtask/src/kvm/evidence.rs').read_text()
ordered(body(hecs_archive, 'write_kvm_success_summary'), ['hecs::evidence(&rustos_log, options.rustos_vcpus)',
    'hardware_execution,', 'publish_success_summary('], 'archived exact-image evidence retains HECS observations')
failure_snapshot = Path('tools/xtask/src/kvm/failure_snapshot.rs').read_text()
ordered(body(failure_snapshot, 'after_failed_probe'), ['if result.is_err()', 'diagnostic();'],
    'only a latched failure may invoke CPU snapshot diagnostics')
smoke = body(Path('tools/xtask/src/kvm/options.rs').read_text(), 'run_one_smoke')
ordered(smoke, ['let result: Result<ProbeResult>', 'wait_for_parallel_boot(',
    'failure_snapshot::after_failed_probe(result, ||', 'options.physical_gpu_bdf.is_none()',
    'failure_snapshot::capture(config, &layout, options.rustos_vcpus)',
    'stop_guest(&mut rustos)', 'stop_guest(&mut dvm)', 'let probe = result?',
    'write_kvm_success_summary('], 'failure-only diagnostic before teardown cannot retry acceptance')
if 'command("cont")' in failure_snapshot or 'collection=partial' not in failure_snapshot:
    raise SystemExit('failure-only snapshot resumes a failed guest or hides partial collection')
rtc = Path('kernel/hal/src/arch/rtc.rs').read_text()
ordered(body(rtc, 'wake_ready_sleepers'), ['notify_ready_sleepers(&RTC_SLEEP_WAITERS, now, crate::hooks::wake_task)',
    'RTC_SLEEP_LOCK_MISSES.fetch_add'], 'exact task deadline notification with nonblocking clockevent retry')
notify = body(rtc, 'notify_ready_sleepers')
ordered(notify, ['try_snapshot_ready_sleep_waiters(waiters, now, &mut ready_tasks)',
    'let admitted = wake(task_id);', 'true'], 'actual tested timer notification path')
if 'remove_task(' in notify or 'no live task' in notify:
    raise SystemExit('clockevent notification/refusal destroyed the original deadline authority')
if 'wake_task: Some(ps_api::wake_task)' not in Path('kernel/executive/src/hal_hooks.rs').read_text():
    raise SystemExit('exact task deadlines cannot route through user-qualified scheduler wake')
tsc_sample = body(Path('apps/ipcbench/src/main.rs').read_text(), 'tsc')
ordered(tsc_sample, ['asm!(', '"lfence",', '"rdtsc",', '"lfence",', 'options(nostack)'],
    'TSC sample has both hardware ordering edges and a compiler memory clobber')
if 'nomem' in tsc_sample:
    raise SystemExit('compiler may move response validation across a sample boundary')
ipcbench = Path('apps/ipcbench/src/ipc_probes.rs').read_text()
for server in ['bench_server', 'bench_server_reply_recv']:
    ordered(body(ipcbench, server), ['received != size_of::<BenchMsg>() as i64',
        'let response = request;', 'SYS_RUSTOS_IPC_REPLY'], 'actual 16-byte echo server: ' + server)
ordered(body(ipcbench, 'validated_round_trip_sample'), ['let start = stamp();',
    'let status = call(request, response);', 'let end = stamp();',
    'valid_round_trip_reply(status, request, response).then_some((start, end))'],
    'actual sample verifies the exact reply outside the measured interval')
ordered(body(ipcbench, 'measure_intra_process_round_trip'), ['for _ in 0..WARMUP',
    'valid_round_trip_reply(call(&request, &mut response), &request, &response)',
    'for _ in 0..IPC_ITERS', 'validated_round_trip_sample(&request, &mut response, &mut call, tsc)',
    'total.push(end.wrapping_sub(start))', 'request.op = BENCH_OP_STOP;',
    'let stopped = valid_round_trip_reply(', 'let joined = server.join().is_ok();',
    'if !round_trip_transcript_complete(ok, stopped, joined, total.len())',
    'skip(name, "ipc-transcript-invalid")', 'return;',
    'report(name, &summarize(&mut total), tsc_khz)'],
    'warmup, every sample, stop and join form one complete production transcript')
print('independent dispatch source contracts passed: whole Scheduler deletion, original dispatch/FIFO, foreign shared-MM census, executor ACK and HECS')
