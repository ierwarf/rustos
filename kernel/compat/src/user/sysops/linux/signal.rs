// RING3-MIGRATION-REFERENCE: Linux signal delivery remains partial. Basic
// signal policy (`rt_sigaction`, `rt_sigprocmask`, `sigaltstack`, `tgkill`) is
// live through syscalld/kernel narrow paths; this reference is only for the
// still-missing user-mode signal-frame delivery path.
//
// enum PendingSignalAction {
//     Ignore(u64),
//     Terminate(u64),
//     UnsupportedHandler(u64),
// }
//
// pub(crate) fn deliver_pending_signals_for_current_thread() {
//     loop {
//         let action = select_next_unmasked_signal_for_current_thread();
//         match action {
//             Some(PendingSignalAction::Ignore(_)) => {}
//             Some(PendingSignalAction::Terminate(signal)) => {
//                 exit_current_process(128 + signal);
//             }
//             Some(PendingSignalAction::UnsupportedHandler(signal)) => {
//                 // TODO: build a Linux-compatible signal frame on the current
//                 // or alternate signal stack and redirect user RIP to the
//                 // installed handler, with rt_sigreturn restoring context.
//                 debug::println!("linux signal handler delivery missing: {}", signal);
//                 exit_current_process(128 + signal);
//             }
//             None => return,
//         }
//     }
// }
