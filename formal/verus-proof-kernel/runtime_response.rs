use vstd::prelude::*;

verus! {

pub const MAX_RUNTIME_PROGRAMS: u64 = 64;

#[derive(PartialEq, Eq)]
pub enum Operation {
    Snapshot,
    Launch,
    Terminate,
    Ready,
    Unknown,
}

#[derive(PartialEq, Eq)]
pub enum Status {
    Ok,
    ServerError,
    PositiveMalformed,
    MinimumMalformed,
}

#[derive(PartialEq, Eq)]
pub enum Outcome {
    Success,
    ServerError,
    ProtocolError,
    Overflow,
}

/// Mathematical proof kernel for the response-admission branch implemented by
/// runtime-control::response_payload_len. The executable client remains the
/// source of truth; this file proves the unbounded state partition that its
/// finite TLC model and Kani harnesses exercise concretely.
pub open spec fn admit_response(
    request: Operation,
    response: Operation,
    status: Status,
    count: u64,
) -> Outcome {
    match status {
        Status::ServerError => Outcome::ServerError,
        Status::PositiveMalformed | Status::MinimumMalformed => Outcome::ProtocolError,
        Status::Ok => {
            if response != request {
                Outcome::ProtocolError
            } else if request == Operation::Snapshot {
                if count <= MAX_RUNTIME_PROGRAMS {
                    Outcome::Success
                } else {
                    Outcome::Overflow
                }
            } else if count == 0 {
                Outcome::Success
            } else {
                Outcome::ProtocolError
            }
        }
    }
}

proof fn exact_success_is_the_only_successful_cross_rpc_outcome(
    request: Operation,
    response: Operation,
    count: u64,
)
    requires response != request,
    ensures admit_response(request, response, Status::Ok, count) != Outcome::Success,
{
}

proof fn malformed_status_cannot_become_success(
    request: Operation,
    response: Operation,
    count: u64,
)
    ensures
        admit_response(request, response, Status::PositiveMalformed, count) != Outcome::Success,
        admit_response(request, response, Status::MinimumMalformed, count) != Outcome::Success,
{
}

proof fn command_success_is_payload_free(
    request: Operation,
    response: Operation,
    count: u64,
)
    requires request != Operation::Snapshot,
             admit_response(request, response, Status::Ok, count) == Outcome::Success,
    ensures count == 0,
{
}

proof fn snapshot_success_is_bounded(
    response: Operation,
    count: u64,
)
    requires admit_response(Operation::Snapshot, response, Status::Ok, count) == Outcome::Success,
    ensures count <= MAX_RUNTIME_PROGRAMS,
{
}

} // verus!

fn main() {}
