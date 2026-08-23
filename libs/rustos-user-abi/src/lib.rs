#![no_std]

#[cfg(feature = "linux-compat-state")]
extern crate alloc;

#[cfg(feature = "linux-compat-state")]
pub mod linux;

pub mod console;
pub mod deadline;
pub mod device;
pub mod ioctl;
pub mod performance;
pub mod syscall;
pub mod ui;
pub mod windows;

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::{console, device, performance, syscall, ui, windows};

    #[test]
    fn ipc_transfer_ticket_wire_is_canonical_and_rejects_zero_authority() {
        let ticket = syscall::IpcTransferTicketWire::new(7, 11, 13).expect("nonzero ticket");
        let bytes = ticket.encode();
        assert_eq!(syscall::IpcTransferTicketWire::decode(&bytes), Some(ticket));

        let mut zero_id = bytes;
        zero_id[..8].fill(0);
        assert!(syscall::IpcTransferTicketWire::decode(&zero_id).is_none());
        let mut zero_nonce = bytes;
        zero_nonce[8..].fill(0);
        assert!(syscall::IpcTransferTicketWire::decode(&zero_nonce).is_none());
        assert!(syscall::IpcTransferTicketWire::decode(&bytes[..15]).is_none());
    }

    #[test]
    fn windows_topology_observation_keeps_reserved_fields_zero() {
        let basic = windows::WindowsSystemBasicInformation::from_online_count(8);
        assert_eq!(basic.reserved1, [0; 24]);
        assert_eq!(basic.reserved2, [0; 4]);
        assert_eq!(basic.number_of_processors, 8);
        assert_eq!(basic.reserved3, [0; 7]);
    }

    #[test]
    fn display_abi_layout_is_stable() {
        assert_eq!(size_of::<device::DisplayInfo>(), 32);
        assert_eq!(size_of::<device::DisplaySurfaceCreate>(), 48);
        assert_eq!(size_of::<device::DisplayPresentRequest>(), 8);
        assert_eq!(size_of::<device::DisplayPresentRectRequest>(), 24);
        assert_eq!(size_of::<device::DisplayGpuInfo>(), 64);
        assert_eq!(size_of::<device::DisplayGpuDamage>(), 16);
        assert_eq!(size_of::<device::DisplayGpuSubmitRequest>(), 40);
        assert_eq!(size_of::<device::DisplayGpuCompletionQuery>(), 264);
    }

    #[test]
    fn console_and_input_abi_layout_is_stable() {
        assert_eq!(size_of::<ui::UiInputEvent>(), 24);
        assert_eq!(size_of::<console::ConsoleStateInfo>(), 16);
        assert_eq!(size_of::<console::ConsoleSessionInfo>(), 72);
        assert_eq!(size_of::<console::ConsoleCreateSessionRequest>(), 48);
    }

    #[test]
    fn performance_limits_are_strictly_layered() {
        const {
            assert!(performance::BOOT_TO_UI_TARGET_MS < performance::BOOT_TO_UI_HARD_LIMIT_MS);
            assert!(
                performance::UI_BOOT_GPU_ACTIVATION_BUDGET_MS
                    < performance::BOOT_TO_UI_HARD_LIMIT_MS
            );
            assert!(
                performance::IPC_READINESS_QUERY_HARD_LIMIT_MS
                    < performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
            );
            assert!(
                performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS
                    < performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS
            );
            assert!(
                performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS
                    < performance::IPC_BULK_DATA_HARD_LIMIT_MS
            );
        }
        assert_eq!(performance::UI_FRAME_MAX_SYNCHRONOUS_POLICY_IPC, 0);
        assert_eq!(performance::IPC_CONTROL_DRAIN_BUDGET, 32);
        assert_eq!(performance::ROOTD_SUPERVISOR_IDLE_POLL_MS, 10);
        assert_eq!(performance::SERVICE_LOOKUP_MAX_IPC_WITH_EXACT_GRANT, 0);
        assert_eq!(
            performance::SERVICE_ENDPOINT_STABLE_LOOKUP_MAX_LOCK_ACQUISITIONS,
            0
        );
    }

    #[test]
    fn loader_abi_layout_fits_inline_ipc() {
        assert_eq!(syscall::IPC_SERVICE_LOADERD, 6);
        assert!(size_of::<syscall::LoaderSpawnRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::LoaderSpawnResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::RustosProcCommitBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(
            size_of::<syscall::RustosProcActivateBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(
            size_of::<syscall::RustosProcActivateBatchBrokerArgs>()
                <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(size_of::<syscall::LoaderActivateBatchRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::LoaderActivateBatchResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(
            size_of::<syscall::RustosIpcWaitServiceEndpointArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(
            size_of::<syscall::RustosProcSetWindowsRuntimeBrokerArgs>()
                <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(
            size_of::<syscall::RustosProcMapFileBatchBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(
            size_of::<syscall::RustosProcSetLinuxRuntimeBrokerArgs>()
                <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert_eq!(syscall::SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER, 0x5255_0030);
        assert_eq!(
            syscall::SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER,
            0x5255_0031
        );
        assert_eq!(syscall::SYS_RUSTOS_DEVICE_OPEN_BROKER, 0x5255_0032);
        assert_eq!(syscall::SYS_RUSTOS_INPUT_INGEST_BROKER, 0x5255_0033);
        assert_eq!(syscall::SYS_RUSTOS_IPC_TRY_RECV, 0x5255_0035);
        assert_eq!(syscall::SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER, 0x5255_0036);
        assert_eq!(syscall::SYS_RUSTOS_IPC_RECV_WITH_SENDER, 0x5255_0038);
        assert_eq!(
            syscall::SYS_RUSTOS_IPC_RECV_WITH_SENDER_BOUNDED,
            0x5255_004a
        );
        assert_eq!(size_of::<syscall::IpcRecvWithSenderArgs>(), 48);
        assert_eq!(syscall::SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT, 0x5255_0039);
        assert_eq!(syscall::SYS_RUSTOS_PROC_ACTIVATE_BROKER, 0x5255_003a);
        assert_eq!(syscall::SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER, 0x5255_0047);
        assert_eq!(syscall::SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER, 0x5255_0048);
        assert_ne!(
            syscall::SYS_RUSTOS_IPC_REPLY_RECV_WITH_SENDER,
            syscall::SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER
        );
        assert_eq!(syscall::SYS_RUSTOS_ROOTD_WAIT_BROKER, 0x5255_003b);
        assert_eq!(syscall::SYS_RUSTOS_ROOTD_TERMINATE_BROKER, 0x5255_003c);
        assert_eq!(
            syscall::SYS_RUSTOS_SCHEDULING_CONTEXT_GRANT_BROKER,
            0x5255_004d
        );
        assert_eq!(syscall::SYS_RUSTOS_SCHEDULING_CONTEXT_SNAPSHOT, 0x5255_004e);
        assert!(
            size_of::<syscall::RustosRootdTerminateBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
    }

    #[test]
    fn procd_abi_layout_fits_inline_ipc() {
        assert_eq!(syscall::IPC_SERVICE_PROCD, 9);
        assert_eq!(syscall::SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER, 0x5255_002a);
        assert!(size_of::<syscall::ProcdIpcRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::ProcdIpcResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(
            size_of::<syscall::RustosProcExecTargetBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(
            size_of::<syscall::RustosProcCancelExecBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(size_of::<syscall::RustosProcForkBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(
            size_of::<syscall::RustosProcSignalQueueBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
    }

    #[test]
    fn service_protocol_abi_layout_fits_inline_ipc() {
        assert_eq!(syscall::IPC_SERVICE_ROOTD, 10);
        assert_eq!(syscall::IPC_SERVICE_SESSIOND, 11);
        assert_eq!(syscall::IPC_SERVICE_PAGERD, 12);
        assert_eq!(syscall::IPC_SERVICE_UISERVER, 14);
        assert_eq!(syscall::IPC_SERVICE_INITD, 15);
        assert_eq!(syscall::DEVMGRD_IPC_OP_OPEN, 3);
        assert_eq!(syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE, 4);
        assert_eq!(syscall::DEVMGRD_IPC_OP_IOCTL_ROUTE, 5);
        assert_eq!(syscall::DEVMGRD_IOCTL_ROUTE_DIRECT, 0);
        assert_eq!(syscall::DEVMGRD_IOCTL_ROUTE_DEVMGRD, 1);
        assert_eq!(syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY, 2);
        assert_eq!(syscall::DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT, 3);
        assert_eq!(syscall::DEVMGRD_IOCTL_LINUX_TTY_TCGETS, 0x5401);
        assert_eq!(syscall::DEVMGRD_IOCTL_LINUX_TTY_TCSETS, 0x5402);
        assert_eq!(syscall::DEVMGRD_IOCTL_LINUX_TTY_TCSETSW, 0x5403);
        assert_eq!(syscall::DEVMGRD_IOCTL_LINUX_TTY_TCSETSF, 0x5404);
        assert_eq!(syscall::DEVMGRD_IOCTL_LINUX_TTY_FIONREAD, 0x541b);
        assert_eq!(syscall::INPUTD_IPC_ABI_VERSION, 5);
        assert_eq!(syscall::VFS_IPC_ABI_VERSION, 5);
        assert_eq!(syscall::VFS_IPC_OP_CURSOR_SETTLE, 24);
        assert_eq!(syscall::VFS_IPC_OP_CHECKPOINT_ACK, 25);
        assert_eq!(
            syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT,
            12
        );
        assert_eq!(syscall::COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE, 13);
        assert_eq!(syscall::NETD_IPC_ABI_VERSION, 7);
        assert_eq!(syscall::SYS_RUSTOS_WAITSET_SIGNAL_BROKER, 0x5255_003f);
        assert_eq!(syscall::SYS_RUSTOS_ENTROPY_BROKER, 0x5255_0040);
        assert_eq!(syscall::SYS_RUSTOS_EARLY_SYSTEM_BROKER, 0x5255_0041);
        assert_eq!(syscall::SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER, 0x5255_0042);
        assert_eq!(syscall::WAITSET_PROVIDER_SESSIOND, 4);
        assert_eq!(syscall::WAITSET_PROVIDER_MAX, 4);
        assert_eq!(size_of::<syscall::WaitSetSignalBrokerArgs>(), 32);
        assert_eq!(size_of::<syscall::WaitSetInterestWire>(), 48);
        assert_eq!(syscall::BLOCK_BROKER_ABI_VERSION, 3);
        assert_eq!(syscall::BLOCK_BROKER_OP_DVM_INFO, 1);
        assert_eq!(syscall::BLOCK_BROKER_OP_DVM_WAIT, 7);
        assert_eq!(size_of::<syscall::RustosBlockBrokerArgs>(), 96);
        assert_eq!(size_of::<syscall::EarlySystemBrokerArgs>(), 144);
        assert_eq!(syscall::INPUTD_IPC_OP_DRAIN_INGEST, 4);
        assert_eq!(syscall::INPUTD_IPC_OP_READ, 5);
        assert_eq!(syscall::INPUTD_INGRESS_KIND_POINTER_PACKET, 2);
        assert_eq!(syscall::INPUTD_INGRESS_KIND_POINTER_POSITION, 3);
        assert_eq!(syscall::INPUTD_INGRESS_KIND_DVM_LINUX_KEY, 10);
        assert_eq!(syscall::INPUTD_INGRESS_FLAG_DVM_SOURCE, 1 << 1);

        assert!(size_of::<syscall::DevmgrdDeviceOpenRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::DevmgrdDeviceOpenResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::DevmgrdDeviceIoctlRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::DevmgrdDeviceIoctlResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::InputIngestBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert_eq!(size_of::<syscall::InputKeyboardEventWire>(), 16);
        assert_eq!(size_of::<syscall::InputPointerPacketWire>(), 12);
        assert_eq!(size_of::<syscall::InputPointerPositionWire>(), 16);
        assert_eq!(size_of::<syscall::InputIngressWire>(), 52);
        assert!(size_of::<syscall::InputdReadResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::NetdIpcRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::NetdIpcResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
    }

    #[test]
    fn commercial_max_protocol_abi_layout_fits_inline_ipc() {
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, 1);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR, 1);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_PROCD, 2);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_LOADERD, 3);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_SYSCALLD, 4);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_VFSD, 5);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_DEVMGRD, 6);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_INPUTD, 7);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_STORAGED, 8);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_NETD, 9);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_SESSIOND, 11);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_PAGERD, 12);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_CAPABILITY, 14);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_UISERVER, 15);
        assert_eq!(syscall::COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL, 5);
        assert_eq!(syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY, 6);
        assert_eq!(syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP, 7);
        assert_eq!(syscall::COMMERCIAL_MAX_PROCD_OP_SESSION_MEMBERSHIP, 7);
        assert_eq!(syscall::COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN, 7);
        assert_eq!(syscall::COMMERCIAL_MAX_SYSCALLD_OP_COLD_SYSCALL_OFFLOAD, 7);
        assert_eq!(syscall::COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY, 6);
        assert_eq!(syscall::COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_OPEN, 2);
        assert_eq!(syscall::COMMERCIAL_MAX_INPUTD_OP_INPUT_READER, 2);
        assert_eq!(syscall::COMMERCIAL_MAX_NETD_OP_PACKET_LEASE, 5);
        assert_eq!(syscall::COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP, 5);
        assert_eq!(
            syscall::COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS,
            0x102
        );
        assert_eq!(syscall::SESSIOND_CONSOLE_READINESS_MASK, 0b11);
        assert_eq!(
            syscall::COMMERCIAL_MAX_UISERVER_OP_TERMINAL_PRESENT_POLICY,
            5
        );
        assert_eq!(syscall::COMMERCIAL_MAX_UISERVER_OP_TRUSTED_UI_STATUS, 6);
        assert_eq!(device::DISPLAY_INFO_FLAG_DVM_SCANOUT, 1 << 2);
        assert_eq!(syscall::COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE, 3);
        assert!(
            size_of::<syscall::CommercialMaxProtocolRequest>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(
            size_of::<syscall::CommercialMaxProtocolResponse>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert!(
            size_of::<syscall::CommercialMaxCapabilityLeaseWire>() <= syscall::IPC_MAX_INLINE_BYTES
        );
    }

    #[test]
    fn mm_broker_abi_layout_is_stable() {
        assert_eq!(syscall::SYS_RUSTOS_MM_BROKER, 0x5255_001e);
        assert_eq!(size_of::<syscall::RustosMmBrokerArgs>(), 80);
        assert_eq!(size_of::<syscall::RustosMmLayoutBrokerResult>(), 48);
        assert_eq!(
            size_of::<syscall::RustosMmFdBrokerResult>(),
            24 + syscall::MM_BROKER_PATH_CAPACITY
        );
        assert_eq!(size_of::<syscall::RustosMmMapBrokerResult>(), 16);
        assert!(size_of::<syscall::RustosMmFdBrokerResult>() <= syscall::IPC_MAX_INLINE_BYTES);
    }
}
