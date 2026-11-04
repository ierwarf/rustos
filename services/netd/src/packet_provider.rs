//! DVM packet provider access through the net broker.

use super::*;

pub(super) fn packet_status() -> Result<u64, i32> {
    call_packet_broker(NET_BROKER_OP_PACKET_STATUS, 0, 0)
}

pub(super) fn packet_provider_state() -> Result<PacketProviderState, i32> {
    packet_provider_state_from_wire(packet_status()?)
}

pub(super) fn packet_provider_state_from_wire(value: u64) -> Result<PacketProviderState, i32> {
    match value {
        NET_BROKER_PACKET_STATUS_UNAVAILABLE => Ok(PacketProviderState::Unavailable),
        NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL => {
            Ok(PacketProviderState::AwaitingAuthenticatedControl)
        }
        NET_BROKER_PACKET_STATUS_ACTIVE => Ok(PacketProviderState::Active),
        _ => Err(libc::EPROTO),
    }
}

pub(super) fn await_authenticated_packet_provider_with<ProviderState, Now, LifecycleRemaining, Sleep>(
    request: &NetdIpcRequest,
    mut provider_state: ProviderState,
    mut now_ns: Now,
    mut lifecycle_remaining: LifecycleRemaining,
    mut sleep: Sleep,
) -> Result<(), i32>
where
    ProviderState: FnMut() -> Result<PacketProviderState, i32>,
    Now: FnMut() -> u64,
    LifecycleRemaining: FnMut() -> Option<Duration>,
    Sleep: FnMut(Duration),
{
    let mut logged_wait = false;
    loop {
        // Check the request and lifecycle before every status transaction.
        // In particular an already expired request cannot issue a status
        // query and then publish a socket from a stale successful result.
        request_deadline_remaining_ns_at(request, now_ns())?;
        if lifecycle_remaining()
            .filter(|remaining| !remaining.is_zero())
            .is_none()
        {
            debug_line("netd: authenticated DVM network control timed out");
            return Err(libc::ENODEV);
        }

        match provider_state()? {
            PacketProviderState::Active => {
                // A status result is not a publication permit by itself: the
                // request may have expired while the broker handled the query.
                request_deadline_remaining_ns_at(request, now_ns())?;
                if lifecycle_remaining()
                    .filter(|remaining| !remaining.is_zero())
                    .is_none()
                {
                    debug_line("netd: authenticated DVM network control timed out");
                    return Err(libc::ENODEV);
                }
                if logged_wait {
                    debug_line("netd: authenticated DVM network control active");
                }
                return Ok(());
            }
            PacketProviderState::Unavailable => {
                debug_line("netd: DVM network transport unavailable");
                return Err(libc::ENODEV);
            }
            PacketProviderState::AwaitingAuthenticatedControl => {
                if !logged_wait {
                    debug_line("netd: waiting for authenticated DVM network control");
                    logged_wait = true;
                }
            }
        }

        // The sleep is bounded simultaneously by the request's exact
        // monotonic end, the four-millisecond retry cadence, and the parent's
        // fresh five-second provider-lifecycle timer.
        let request_wait = capped_request_wait_at(request, now_ns(), AUTHENTICATED_CONTROL_RETRY)?;
        let lifecycle_left = lifecycle_remaining()
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                debug_line("netd: authenticated DVM network control timed out");
                libc::ENODEV
            })?;
        sleep(request_wait.min(lifecycle_left));
    }
}

pub(super) fn await_authenticated_packet_provider(request: &NetdIpcRequest) -> Result<(), i32> {
    let deadline = StdInstant::now() + AUTHENTICATED_CONTROL_WAIT;
    await_authenticated_packet_provider_with(
        request,
        packet_provider_state,
        rustos_svc_runtime::syscall::monotonic_nanos,
        || deadline.checked_duration_since(StdInstant::now()),
        thread::sleep,
    )
}

pub(super) fn packet_tx(frame: &[u8]) -> Result<usize, i32> {
    call_packet_broker(
        NET_BROKER_OP_PACKET_TX,
        frame.as_ptr() as u64,
        frame.len() as u64,
    )
    .map(|value| value as usize)
}

pub(super) fn packet_rx(frame: &mut [u8]) -> Result<usize, i32> {
    call_packet_broker(
        NET_BROKER_OP_PACKET_RX,
        frame.as_mut_ptr() as u64,
        frame.len() as u64,
    )
    .map(|value| value as usize)
}

pub(super) fn call_packet_broker(op: u16, arg0: u64, arg1: u64) -> Result<u64, i32> {
    let args = RustosNetBrokerArgs {
        process_id: 1,
        op,
        reserved0: 0,
        reserved1: 0,
        arg0,
        arg1,
        arg2: 0,
        arg3: 0,
        arg4: 0,
        arg5: 0,
    };
    let result = syscall1(
        SYS_RUSTOS_NET_BROKER,
        (&args as *const RustosNetBrokerArgs) as u64,
    );
    if result < 0 {
        Err(last_errno())
    } else {
        Ok(result as u64)
    }
}
