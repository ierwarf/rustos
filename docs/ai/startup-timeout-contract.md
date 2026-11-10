# Startup and recovery timeout ownership

The functional 120-second startup ceiling used by initd and the interactive
harness is diagnostic, not the product's 10-second qualification SLO. Passing
this ceiling is not product performance evidence. Initd starts one deadline at
process entry; initial rootd reconciliation and update calls, scheduling grants,
loader spawn, lease reports, cohort activation and endpoint admission use the
remaining time, with narrower operation limits retained. Unadmitted startup
entries, including those never launched because a dependency did not become
ready, fail closed with a typed phase, executable and errno. Already admitted
packages retain ordinary per-operation supervision on later restarts. Uiserver
measures each dependency's budget from its startup epoch, including dependencies
whose workers never started. A published required failure remains terminal.

The interactive harness bounds startup and typed display recovery, but leaves a
healthy ready session idle indefinitely. The display owner publishes
`rustos-dvm-display: recovery v=1 generation=<n> attempt=<n> state=<starting|recovering|ready|failed> cause=<stage> errno=<n>`.
Generation zero is pre-invitation and cannot be ready or override any bound
session. A nonzero generation is the GUI pool invitation generation. The latest
bound generation and attempt govern readiness; a pre-invitation terminal failure
is actionable when no bound session exists. A bound ready record attests a
presented RustOS frame and follows that attempt's starting/recovering record and
frame evidence. The harness scopes compositor evidence to that attempt; old
records cannot make a newer attempt ready. Failed is terminal; recovering is
bounded.

Input relay setup uses a single absolute deadline across accept, authentication,
all framed probe requests and replies, receiver admission and session-start
publication. Nonblocking socket transfers recheck that deadline on every
fragment and poll, so progress cannot remint the budget. A live established
input stream remains idle without a deadline. Loader client cancellation is
owned by the shared service runtime and is clipped to the original request
remainder; an expired work lease is retired independently by loaderd/vfsd rather
than creating another cleanup RPC budget. A successfully delivered terminal
spawn response transfers PID custody even if its caller is next scheduled
after expiry; silently discarding that identity would orphan the child.
