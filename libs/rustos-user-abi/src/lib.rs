#![no_std]

#[cfg(feature = "linux-compat-state")]
extern crate alloc;

#[cfg(feature = "linux-compat-state")]
pub mod linux;

pub mod console;
pub mod device;
pub mod ioctl;
pub mod syscall;
pub mod ui;

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::{console, device, syscall, ui};

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
    fn loader_abi_layout_fits_inline_ipc() {
        assert_eq!(syscall::IPC_SERVICE_LOADERD, 6);
        assert!(size_of::<syscall::LoaderSpawnRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::LoaderSpawnResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::RustosProcCommitBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(
            size_of::<syscall::RustosProcActivateBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
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
        assert_eq!(syscall::SYS_RUSTOS_BOOT_EXTENT_BROKER, 0x5255_0034);
        assert_eq!(syscall::SYS_RUSTOS_IPC_TRY_RECV, 0x5255_0035);
        assert_eq!(syscall::SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER, 0x5255_0036);
        assert_eq!(syscall::SYS_RUSTOS_DRIVER_SYMBOL_EVENT_BROKER, 0x5255_0037);
        assert_eq!(syscall::SYS_RUSTOS_IPC_RECV_WITH_SENDER, 0x5255_0038);
        assert_eq!(syscall::SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT, 0x5255_0039);
        assert_eq!(syscall::SYS_RUSTOS_PROC_ACTIVATE_BROKER, 0x5255_003a);
        assert_eq!(syscall::SYS_RUSTOS_ROOTD_WAIT_BROKER, 0x5255_003b);
        assert_eq!(syscall::SYS_RUSTOS_ROOTD_TERMINATE_BROKER, 0x5255_003c);
        assert!(
            size_of::<syscall::RustosRootdTerminateBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES
        );
        assert_eq!(
            syscall::SYS_RUSTOS_SERVICE_DRIVER_RESOURCE_BROKER,
            0x5255_0022
        );
        assert_eq!(size_of::<syscall::LinuxDriverSymbolEventWire>(), 192);
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
        assert_eq!(syscall::IPC_SERVICE_SERVICE_DRIVERD, 13);
        assert_eq!(syscall::IPC_SERVICE_UISERVER, 14);
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
        assert_eq!(syscall::INPUTD_IPC_ABI_VERSION, 3);
        assert_eq!(syscall::VFS_IPC_ABI_VERSION, 3);
        assert_eq!(syscall::VFS_IPC_OP_CURSOR_SETTLE, 24);
        assert_eq!(syscall::VFS_IPC_OP_CHECKPOINT_ACK, 25);
        assert_eq!(
            syscall::COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT,
            12
        );
        assert_eq!(syscall::COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE, 13);
        assert_eq!(syscall::NETD_IPC_ABI_VERSION, 4);
        assert_eq!(syscall::SYS_RUSTOS_WAITSET_SIGNAL_BROKER, 0x5255_003f);
        assert_eq!(syscall::SYS_RUSTOS_ENTROPY_BROKER, 0x5255_0040);
        assert_eq!(syscall::WAITSET_PROVIDER_SESSIOND, 4);
        assert_eq!(syscall::WAITSET_PROVIDER_MAX, 4);
        assert_eq!(size_of::<syscall::WaitSetSignalBrokerArgs>(), 32);
        assert_eq!(size_of::<syscall::WaitSetInterestWire>(), 48);
        assert_eq!(syscall::INPUTD_IPC_OP_DRAIN_INGEST, 4);
        assert_eq!(syscall::INPUTD_IPC_OP_READ, 5);
        assert_eq!(syscall::INPUTD_INGRESS_KIND_POINTER_PACKET, 2);
        assert_eq!(syscall::INPUTD_INGRESS_KIND_POINTER_POSITION, 3);
        assert_eq!(syscall::INPUTD_INGRESS_KIND_DVM_LINUX_KEY, 10);
        assert_eq!(syscall::INPUTD_INGRESS_FLAG_DVM_SOURCE, 1 << 1);
        assert_eq!(syscall::STORAGED_POLICY_ABI_VERSION, 1);

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
        assert_eq!(size_of::<syscall::StoragedAhciPolicyWire>(), 32);
        assert_eq!(size_of::<syscall::StoragedNvmePolicyWire>(), 36);
        assert!(size_of::<syscall::RustosBootExtentBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES);
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
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_DRIVERD, 10);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_SESSIOND, 11);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_PAGERD, 12);
        assert_eq!(syscall::COMMERCIAL_MAX_PROTOCOL_SERVICE_DRIVERD, 13);
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
        assert_eq!(syscall::COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE, 4);
        assert_eq!(syscall::COMMERCIAL_MAX_STORAGED_OP_AHCI_POLICY, 6);
        assert_eq!(syscall::COMMERCIAL_MAX_STORAGED_OP_NVME_POLICY, 7);
        assert_eq!(syscall::COMMERCIAL_MAX_NETD_OP_PACKET_LEASE, 5);
        assert_eq!(syscall::COMMERCIAL_MAX_DRIVERD_OP_PROVIDER_SELECT, 4);
        assert_eq!(syscall::COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP, 5);
        assert_eq!(
            syscall::COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS,
            0x102
        );
        assert_eq!(syscall::SESSIOND_CONSOLE_READINESS_MASK, 0b11);
        assert_eq!(syscall::COMMERCIAL_MAX_SERVICE_DRIVERD_OP_MMIO_LEASE, 2);
        assert_eq!(syscall::COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IRQ_ROUTE, 3);
        assert_eq!(syscall::COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DMA_BUFFER, 4);
        assert_eq!(syscall::COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IO_PORT_LEASE, 5);
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
        assert_eq!(size_of::<syscall::ServiceDriverMmioLeaseWire>(), 32);
        assert_eq!(size_of::<syscall::ServiceDriverIrqRouteWire>(), 24);
        assert_eq!(size_of::<syscall::ServiceDriverDmaBufferWire>(), 32);
        assert_eq!(size_of::<syscall::ServiceDriverIoPortLeaseWire>(), 16);
        assert_eq!(size_of::<syscall::ServiceDriverIoPortValueWire>(), 8);
        assert_eq!(
            size_of::<syscall::RustosServiceDriverResourceBrokerArgs>(),
            72
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
