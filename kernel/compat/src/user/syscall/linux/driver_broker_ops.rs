// Reserved privileged-resource broker for a future non-.ko service driver.
// Linux module loading is intentionally absent: DVM owns all .ko execution.
use super::*;

use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_SERVICE_DRIVER_POLICY, RustosServiceDriverResourceBrokerArgs,
    SERVICE_DRIVER_RESOURCE_BROKER_ABI_VERSION, SERVICE_DRIVER_RESOURCE_OP_DMA_BUFFER,
    SERVICE_DRIVER_RESOURCE_OP_IO_PORT_LEASE, SERVICE_DRIVER_RESOURCE_OP_IO_PORT_READ,
    SERVICE_DRIVER_RESOURCE_OP_IO_PORT_WRITE, SERVICE_DRIVER_RESOURCE_OP_IRQ_ROUTE,
    SERVICE_DRIVER_RESOURCE_OP_MMIO_LEASE, ServiceDriverDmaBufferWire,
    ServiceDriverIoPortLeaseWire, ServiceDriverIoPortValueWire, ServiceDriverIrqRouteWire,
    ServiceDriverMmioLeaseWire,
};
use x86_64::instructions::port::Port;

const SERVICE_DRIVER_MMIO_CACHE_DEVICE: u32 = 1;
const SERVICE_DRIVER_DMA_DEFAULT_ALIGNMENT: u64 = 4096;

pub(super) fn syscall_linux_rustos_service_driver_resource_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_SERVICE_DRIVER_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosServiceDriverResourceBrokerArgs>(
        args_ptr,
    ) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != SERVICE_DRIVER_RESOURCE_BROKER_ABI_VERSION || args.reserved0 != 0 {
        return linux_errno(LINUX_EINVAL);
    }

    let result = match args.op {
        SERVICE_DRIVER_RESOURCE_OP_MMIO_LEASE => broker_service_driver_mmio_lease(&args),
        SERVICE_DRIVER_RESOURCE_OP_IRQ_ROUTE => broker_service_driver_irq_route(&args),
        SERVICE_DRIVER_RESOURCE_OP_DMA_BUFFER => broker_service_driver_dma_buffer(&args),
        SERVICE_DRIVER_RESOURCE_OP_IO_PORT_LEASE => broker_service_driver_io_port_lease(&args),
        SERVICE_DRIVER_RESOURCE_OP_IO_PORT_READ => broker_service_driver_io_port_read(&args),
        SERVICE_DRIVER_RESOURCE_OP_IO_PORT_WRITE => broker_service_driver_io_port_write(&args),
        _ => Err(LINUX_EINVAL),
    };
    match result {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

fn broker_service_driver_mmio_lease(
    args: &RustosServiceDriverResourceBrokerArgs,
) -> Result<(), i64> {
    if args.arg0 == 0 || args.arg1 == 0 {
        return Err(LINUX_EINVAL);
    }
    let lease = ServiceDriverMmioLeaseWire {
        phys_start: args.arg0,
        byte_len: args.arg1,
        cache_policy: SERVICE_DRIVER_MMIO_CACHE_DEVICE,
        flags: args.flags,
        lease_id: service_driver_resource_id(args),
    };
    write_resource_out(args, &lease)
}

fn broker_service_driver_irq_route(
    args: &RustosServiceDriverResourceBrokerArgs,
) -> Result<(), i64> {
    if args.arg0 > u64::from(u32::MAX) || args.arg1 > u64::from(u32::MAX) {
        return Err(LINUX_EINVAL);
    }
    let route = ServiceDriverIrqRouteWire {
        irq: args.arg0 as u32,
        vector: args.arg1 as u32,
        flags: args.flags,
        reserved0: 0,
        route_id: service_driver_resource_id(args),
    };
    write_resource_out(args, &route)
}

fn broker_service_driver_dma_buffer(
    args: &RustosServiceDriverResourceBrokerArgs,
) -> Result<(), i64> {
    if args.arg0 == 0 || args.arg1 != 0 && !args.arg1.is_power_of_two() {
        return Err(LINUX_EINVAL);
    }
    let buffer = ServiceDriverDmaBufferWire {
        byte_len: args.arg0,
        alignment: if args.arg1 == 0 {
            SERVICE_DRIVER_DMA_DEFAULT_ALIGNMENT
        } else {
            args.arg1
        },
        flags: args.flags,
        reserved0: 0,
        buffer_id: service_driver_resource_id(args),
    };
    write_resource_out(args, &buffer)
}

fn broker_service_driver_io_port_lease(
    args: &RustosServiceDriverResourceBrokerArgs,
) -> Result<(), i64> {
    if args.arg0 > u64::from(u16::MAX)
        || args.arg1 == 0
        || args.arg1 > u64::from(u16::MAX)
        || args.arg0.saturating_add(args.arg1 - 1) > u64::from(u16::MAX)
    {
        return Err(LINUX_EINVAL);
    }
    let lease = ServiceDriverIoPortLeaseWire {
        port_start: args.arg0 as u16,
        port_count: args.arg1 as u16,
        flags: args.flags,
        lease_id: service_driver_resource_id(args),
    };
    write_resource_out(args, &lease)
}

fn broker_service_driver_io_port_read(
    args: &RustosServiceDriverResourceBrokerArgs,
) -> Result<(), i64> {
    let port = checked_io_port(args.arg0)?;
    let width = checked_io_width(args.arg1)?;
    let value = unsafe { read_io_port(port, width) };
    let result = ServiceDriverIoPortValueWire {
        value,
        width,
        reserved0: 0,
    };
    write_resource_out(args, &result)
}

fn broker_service_driver_io_port_write(
    args: &RustosServiceDriverResourceBrokerArgs,
) -> Result<(), i64> {
    let port = checked_io_port(args.arg0)?;
    let width = checked_io_width(args.arg2)?;
    unsafe {
        write_io_port(port, width, args.arg1)?;
    }
    Ok(())
}

fn checked_io_port(port: u64) -> Result<u16, i64> {
    u16::try_from(port).map_err(|_| LINUX_EINVAL)
}

fn checked_io_width(width: u64) -> Result<u16, i64> {
    match width {
        1 | 2 | 4 => Ok(width as u16),
        _ => Err(LINUX_EINVAL),
    }
}

unsafe fn read_io_port(port: u16, width: u16) -> u32 {
    match width {
        1 => {
            let mut port = Port::<u8>::new(port);
            unsafe { u32::from(port.read()) }
        }
        2 => {
            let mut port = Port::<u16>::new(port);
            unsafe { u32::from(port.read()) }
        }
        4 => {
            let mut port = Port::<u32>::new(port);
            unsafe { port.read() }
        }
        _ => 0,
    }
}

unsafe fn write_io_port(port: u16, width: u16, value: u64) -> Result<(), i64> {
    match width {
        1 => {
            let value = u8::try_from(value).map_err(|_| LINUX_EINVAL)?;
            let mut port = Port::<u8>::new(port);
            unsafe { port.write(value) };
        }
        2 => {
            let value = u16::try_from(value).map_err(|_| LINUX_EINVAL)?;
            let mut port = Port::<u16>::new(port);
            unsafe { port.write(value) };
        }
        4 => {
            let value = u32::try_from(value).map_err(|_| LINUX_EINVAL)?;
            let mut port = Port::<u32>::new(port);
            unsafe { port.write(value) };
        }
        _ => return Err(LINUX_EINVAL),
    }
    Ok(())
}

fn write_resource_out<T: Copy>(
    args: &RustosServiceDriverResourceBrokerArgs,
    value: &T,
) -> Result<(), i64> {
    if args.out_ptr == 0 {
        return Ok(());
    }
    if args.out_len < core::mem::size_of::<T>() as u64 {
        return Err(LINUX_EINVAL);
    }
    usermem::write_current_user_struct(args.out_ptr, value)
        .map_err(address_space_error_to_linux_errno)
}

fn service_driver_resource_id(args: &RustosServiceDriverResourceBrokerArgs) -> u64 {
    (u64::from(args.op) << 48)
        ^ (args.subject_pid << 16)
        ^ args.subject_tid
        ^ args.arg0.rotate_left(7)
        ^ args.arg1
        ^ args.arg2.rotate_left(17)
}
