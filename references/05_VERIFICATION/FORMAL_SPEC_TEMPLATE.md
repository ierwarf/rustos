# 형식 명세 템플릿

## System boundary
- Components:
- Trusted components:
- Untrusted environment:
- Hardware assumptions:

## State
- Objects and identifiers:
- Ownership/capabilities:
- Lifecycle states:
- Queues and pending operations:
- Generations/epochs:

## Actions
- Create:
- Publish:
- Invoke:
- Cancel:
- Timeout:
- Crash:
- Restart:
- Revoke:
- Destroy:

## Safety invariants
- No use after revoke:
- No unauthorized information flow:
- At most one device owner:
- Published objects are fully initialized:
- Queue accounting is conserved:
- ABI-visible state changes atomically where required:

## Liveness
- Every accepted request eventually completes or returns a terminal error.
- Revocation eventually prevents new use.
- A crashed service can be restarted without reusing stale generation handles.

## Fairness assumptions
- Scheduler fairness:
- Interrupt delivery:
- Device completion:
- Network/storage availability:

## Refinement mapping
- Spec state → implementation fields:
- Spec action → code entry points:
- Runtime assertions:
- Tests generated from traces:
