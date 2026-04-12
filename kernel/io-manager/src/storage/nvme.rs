use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use spin::Mutex;
use storage_core::BlockDevice as SharedBlockDevice;

use crate::arch::pci::PciDevice;
use crate::storage::block::{BlockDeviceOps, BlockTransportKind};
use crate::storage::fat::{DiskIoError, IoResult};

const PCI_CLASS_MASS_STORAGE: u8 = 0x01;
const PCI_SUBCLASS_NVM: u8 = 0x08;
const PCI_PROG_IF_NVME: u8 = 0x02;
const NVME_BAR_INDEX: usize = 0;

const NVME_VS: usize = 0x08;
const NVME_INTMS: usize = 0x0c;
const NVME_INTMC: usize = 0x10;
const NVME_CC: usize = 0x14;
const NVME_CSTS: usize = 0x1c;
const NVME_AQA: usize = 0x24;
const NVME_ASQ: usize = 0x28;
const NVME_ACQ: usize = 0x30;
const NVME_DOORBELL_BASE: usize = 0x1000;

const NVME_CC_EN: u32 = 1 << 0;
const NVME_CC_IOSQES_SHIFT: u32 = 16;
const NVME_CC_IOCQES_SHIFT: u32 = 20;
const NVME_CSTS_RDY: u32 = 1 << 0;

const NVME_ADMIN_OP_CREATE_IO_SQ: u8 = 0x01;
const NVME_ADMIN_OP_CREATE_IO_CQ: u8 = 0x05;
const NVME_ADMIN_OP_IDENTIFY: u8 = 0x06;
const NVME_ADMIN_OP_SET_FEATURES: u8 = 0x09;
const NVME_NVM_OP_FLUSH: u8 = 0x00;
const NVME_NVM_OP_WRITE: u8 = 0x01;
const NVME_NVM_OP_READ: u8 = 0x02;

const NVME_IDENTIFY_CNS_NAMESPACE: u32 = 0x00;
const NVME_IDENTIFY_CNS_CONTROLLER: u32 = 0x01;
const NVME_FEATURE_NUM_QUEUES: u32 = 0x07;

const NVME_ADMIN_QUEUE_DEPTH: u16 = 16;
const NVME_IO_QUEUE_DEPTH: u16 = 16;
const NVME_IDENTIFY_BYTES: usize = 4096;
const NVME_DATA_BUFFER_BYTES: usize = 4096;
const NVME_WAIT_SPINS: usize = 5_000_000;

fn emit_nvme(level: diag_abi::DiagLevel, event_id: u16, object_id: u64, message: String) {
    crate::debug::emit_text(
        diag_abi::DiagProvider::Io,
        level,
        event_id,
        0,
        object_id,
        message.as_str(),
    );
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvmeSubmission {
    cdw0: u32,
    nsid: u32,
    rsvd2: u64,
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvmeCompletion {
    result: u32,
    rsvd: u32,
    sq_head: u16,
    sq_id: u16,
    cid: u16,
    status: u16,
}

struct NvmeController {
    mmio_base: usize,
    mmio_len: usize,
    dma_key: *mut c_void,
    doorbell_stride: usize,
    #[allow(dead_code)]
    version: u32,
}

unsafe impl Send for NvmeController {}
unsafe impl Sync for NvmeController {}

struct NvmeQueue {
    qid: u16,
    sq_cpu: *mut NvmeSubmission,
    sq_dma: u64,
    cq_cpu: *mut NvmeCompletion,
    cq_dma: u64,
    entry_count: u16,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: u16,
    next_cid: u16,
}

unsafe impl Send for NvmeQueue {}

struct NvmeRuntime {
    #[allow(dead_code)]
    admin: NvmeQueue,
    io: NvmeQueue,
    #[allow(dead_code)]
    identify_cpu: *mut u8,
    #[allow(dead_code)]
    identify_dma: u64,
    data_cpu: *mut u8,
    data_dma: u64,
}

unsafe impl Send for NvmeRuntime {}

struct NvmeBlockDevice {
    controller: Arc<NvmeController>,
    namespace_id: u32,
    block_count: u64,
    logical_block_size: usize,
    #[allow(dead_code)]
    model: String,
    runtime: Mutex<NvmeRuntime>,
}

unsafe impl Send for NvmeBlockDevice {}
unsafe impl Sync for NvmeBlockDevice {}

pub(crate) fn probe_devices() -> Vec<Box<dyn BlockDeviceOps>> {
    let mut devices = Vec::new();
    crate::arch::pci::visit_devices(|pci| {
        let vendor = pci.vendor_id();
        let device = pci.device_id();
        let class_code = pci.class_code();
        let subclass = pci.subclass();
        let prog_if = pci.prog_if();
        let qemu_nvme = vendor == 0x1b36 && device == 0x0010;

        if (class_code == PCI_CLASS_MASS_STORAGE || qemu_nvme)
            && crate::debug::should_emit(diag_abi::DiagProvider::Io, diag_abi::DiagLevel::Debug)
        {
            emit_nvme(
                diag_abi::DiagLevel::Debug,
                0,
                0,
                format!(
                    "nvme: pci {:02x}:{:02x}.{} vendor={:04x} device={:04x} class={:02x}/{:02x}/{:02x}",
                    pci.bus,
                    pci.device,
                    pci.function,
                    vendor,
                    device,
                    class_code,
                    subclass,
                    prog_if
                ),
            );
        }

        if !(class_code == PCI_CLASS_MASS_STORAGE
            && subclass == PCI_SUBCLASS_NVM
            && prog_if == PCI_PROG_IF_NVME)
            && !qemu_nvme
        {
            return false;
        }
        match probe_controller(pci) {
            Ok(Some(device)) => devices.push(Box::new(device) as Box<dyn BlockDeviceOps>),
            Ok(None) => {}
            Err(_err)
                if crate::debug::should_emit(
                    diag_abi::DiagProvider::Io,
                    diag_abi::DiagLevel::Warn,
                ) =>
            {
                emit_nvme(
                    diag_abi::DiagLevel::Warn,
                    1,
                    0,
                    format!(
                        "nvme: controller {:02x}:{:02x}.{} skipped: {:?}",
                        pci.bus, pci.device, pci.function, _err
                    ),
                );
            }
            Err(_) => {}
        }
        false
    });
    devices
}

impl SharedBlockDevice for NvmeBlockDevice {
    fn logical_block_size(&self) -> usize {
        self.logical_block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        let trace =
            crate::debug::should_emit(diag_abi::DiagProvider::Io, diag_abi::DiagLevel::Debug);
        if trace {
            emit_nvme(
                diag_abi::DiagLevel::Debug,
                2,
                self.namespace_id as u64,
                format!(
                    "nvme read_blocks: begin nsid={} lba={} bytes={} block_size={} blocks={}",
                    self.namespace_id,
                    lba,
                    out.len(),
                    self.logical_block_size,
                    self.block_count
                ),
            );
        }
        if out.is_empty() || out.len() % self.logical_block_size != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let mut runtime = self.runtime.lock();
        let mut offset = 0usize;
        while offset < out.len() {
            let block_offset = offset / self.logical_block_size;
            let block_lba = lba
                .checked_add(block_offset as u64)
                .ok_or(DiskIoError::InvalidInput)?;
            let max_bytes = (out.len() - offset).min(NVME_DATA_BUFFER_BYTES);
            let transfer_bytes = (max_bytes / self.logical_block_size) * self.logical_block_size;
            if transfer_bytes == 0 {
                return Err(DiskIoError::InvalidInput);
            }
            let transfer_blocks = transfer_bytes / self.logical_block_size;
            if block_lba
                .checked_add(transfer_blocks as u64)
                .ok_or(DiskIoError::InvalidInput)?
                > self.block_count
            {
                return Err(DiskIoError::InvalidInput);
            }
            let data_dma = runtime.data_dma;
            issue_nvm_command(
                self.controller.as_ref(),
                &mut runtime.io,
                self.namespace_id,
                NVME_NVM_OP_READ,
                block_lba,
                data_dma,
                transfer_blocks as u16,
            )?;
            unsafe {
                crate::arch::simd::copy_fast(
                    runtime.data_cpu,
                    out[offset..offset + transfer_bytes].as_mut_ptr(),
                    transfer_bytes,
                );
            }
            offset += transfer_bytes;
        }
        if trace {
            emit_nvme(
                diag_abi::DiagLevel::Debug,
                3,
                self.namespace_id as u64,
                format!(
                    "nvme read_blocks: end nsid={} lba={} ok=true",
                    self.namespace_id, lba
                ),
            );
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        if input.is_empty() || input.len() % self.logical_block_size != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let mut runtime = self.runtime.lock();
        let mut offset = 0usize;
        while offset < input.len() {
            let block_offset = offset / self.logical_block_size;
            let block_lba = lba
                .checked_add(block_offset as u64)
                .ok_or(DiskIoError::InvalidInput)?;
            let max_bytes = (input.len() - offset).min(NVME_DATA_BUFFER_BYTES);
            let transfer_bytes = (max_bytes / self.logical_block_size) * self.logical_block_size;
            if transfer_bytes == 0 {
                return Err(DiskIoError::InvalidInput);
            }
            let transfer_blocks = transfer_bytes / self.logical_block_size;
            if block_lba
                .checked_add(transfer_blocks as u64)
                .ok_or(DiskIoError::InvalidInput)?
                > self.block_count
            {
                return Err(DiskIoError::InvalidInput);
            }
            unsafe {
                crate::arch::simd::copy_fast(
                    input[offset..offset + transfer_bytes].as_ptr(),
                    runtime.data_cpu,
                    transfer_bytes,
                );
            }
            let data_dma = runtime.data_dma;
            issue_nvm_command(
                self.controller.as_ref(),
                &mut runtime.io,
                self.namespace_id,
                NVME_NVM_OP_WRITE,
                block_lba,
                data_dma,
                transfer_blocks as u16,
            )?;
            offset += transfer_bytes;
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        let mut runtime = self.runtime.lock();
        let cmd = NvmeSubmission {
            cdw0: build_cdw0(NVME_NVM_OP_FLUSH, runtime.io.alloc_cid()),
            nsid: self.namespace_id,
            rsvd2: 0,
            mptr: 0,
            prp1: 0,
            prp2: 0,
            cdw10: 0,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        submit_and_wait(self.controller.as_ref(), &mut runtime.io, cmd).map(|_| ())
    }
}

impl BlockDeviceOps for NvmeBlockDevice {
    fn transport_kind(&self) -> BlockTransportKind {
        BlockTransportKind::Nvme
    }

    fn readonly(&self) -> bool {
        false
    }
}

impl NvmeController {
    fn write_u32(&self, offset: usize, value: u32) {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::write_volatile((self.mmio_base + offset) as *mut u32, value) };
    }

    fn write_u64(&self, offset: usize, value: u64) {
        debug_assert!(offset + 8 <= self.mmio_len);
        unsafe { ptr::write_volatile((self.mmio_base + offset) as *mut u64, value) };
    }

    fn read_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::read_volatile((self.mmio_base + offset) as *const u32) }
    }

    fn doorbell_offset(&self, qid: u16, is_cq: bool) -> usize {
        let index = qid as usize * 2 + if is_cq { 1 } else { 0 };
        NVME_DOORBELL_BASE + index * self.doorbell_stride
    }

    fn ring_sq(&self, queue: &NvmeQueue) {
        self.write_u32(self.doorbell_offset(queue.qid, false), queue.sq_tail as u32);
    }

    fn ring_cq(&self, queue: &NvmeQueue) {
        self.write_u32(self.doorbell_offset(queue.qid, true), queue.cq_head as u32);
    }

    fn disable(&self) -> IoResult<()> {
        let cc = self.read_u32(NVME_CC);
        if (cc & NVME_CC_EN) != 0 {
            self.write_u32(NVME_CC, cc & !NVME_CC_EN);
            if !wait_until(|| (self.read_u32(NVME_CSTS) & NVME_CSTS_RDY) == 0) {
                return Err(DiskIoError::Timeout);
            }
        }
        Ok(())
    }

    fn configure_admin_queue(&self, admin: &NvmeQueue) -> IoResult<()> {
        self.disable()?;
        self.write_u32(NVME_INTMS, u32::MAX);
        self.write_u32(NVME_INTMC, u32::MAX);
        self.write_u32(
            NVME_AQA,
            ((admin.entry_count as u32 - 1) << 16) | (admin.entry_count as u32 - 1),
        );
        self.write_u64(NVME_ASQ, admin.sq_dma);
        self.write_u64(NVME_ACQ, admin.cq_dma);
        let cc = NVME_CC_EN | (6 << NVME_CC_IOSQES_SHIFT) | (4 << NVME_CC_IOCQES_SHIFT);
        self.write_u32(NVME_CC, cc);
        if !wait_until(|| (self.read_u32(NVME_CSTS) & NVME_CSTS_RDY) != 0) {
            return Err(DiskIoError::Timeout);
        }
        Ok(())
    }
}

impl NvmeQueue {
    fn alloc_cid(&mut self) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cid
    }
}

fn probe_controller(pci: PciDevice) -> Result<Option<NvmeBlockDevice>, DiskIoError> {
    let bar = pci
        .resource(NVME_BAR_INDEX)
        .filter(|resource| !resource.is_io && resource.size >= 0x2000)
        .ok_or(DiskIoError::NotPresent)?;

    pci.enable_memory_bus_master();
    let mmio = crate::driver::mmio::map(bar.start, bar.size as usize, false);
    if mmio.is_null() {
        return Err(DiskIoError::NotPresent);
    }

    let cap = unsafe {
        let low = ptr::read_volatile(mmio.cast::<u32>()) as u64;
        let high = ptr::read_volatile((mmio as usize + 4) as *const u32) as u64;
        low | (high << 32)
    };
    let dstrd = ((cap >> 32) & 0xf) as usize;
    let mpsmin = ((cap >> 48) & 0xf) as u8;
    if mpsmin > 0 {
        return Err(DiskIoError::Unsupported);
    }

    let controller = Arc::new(NvmeController {
        mmio_base: mmio as usize,
        mmio_len: bar.size as usize,
        dma_key: mmio,
        doorbell_stride: 4 << dstrd,
        version: unsafe { ptr::read_volatile((mmio as usize + NVME_VS) as *const u32) },
    });
    crate::driver::dma::set_mask_and_coherent(controller.dma_key, u64::MAX);

    let mut admin = allocate_queue(controller.dma_key, 0, NVME_ADMIN_QUEUE_DEPTH)?;
    controller.configure_admin_queue(&admin)?;
    let identify_cpu = alloc_dma_buffer(controller.dma_key, NVME_IDENTIFY_BYTES)?;
    let identify_dma = crate::memory::paging::kernel_virtual_to_physical_addr(identify_cpu as u64);
    let data_cpu = alloc_dma_buffer(controller.dma_key, NVME_DATA_BUFFER_BYTES)?;
    let data_dma = crate::memory::paging::kernel_virtual_to_physical_addr(data_cpu as u64);

    let model =
        identify_controller_model(controller.as_ref(), &mut admin, identify_dma, identify_cpu)?;
    let io = allocate_queue(controller.dma_key, 1, NVME_IO_QUEUE_DEPTH)?;
    configure_io_queues(controller.as_ref(), &mut admin, &io)?;
    let (block_count, logical_block_size, namespace_id) =
        identify_namespace(controller.as_ref(), &mut admin, identify_dma, identify_cpu)?;
    if logical_block_size < 512 {
        return Err(DiskIoError::Unsupported);
    }
    if logical_block_size > NVME_DATA_BUFFER_BYTES {
        return Err(DiskIoError::Unsupported);
    }

    if crate::debug::should_emit(diag_abi::DiagProvider::Io, diag_abi::DiagLevel::Info) {
        emit_nvme(
            diag_abi::DiagLevel::Info,
            4,
            namespace_id as u64,
            format!(
                "nvme: controller {:02x}:{:02x}.{} version={:#x} blocks={} block_size={} model={}",
                pci.bus,
                pci.device,
                pci.function,
                controller.version,
                block_count,
                logical_block_size,
                model
            ),
        );
    }

    Ok(Some(NvmeBlockDevice {
        controller,
        namespace_id,
        block_count,
        logical_block_size,
        model,
        runtime: Mutex::new(NvmeRuntime {
            admin,
            io,
            identify_cpu,
            identify_dma,
            data_cpu,
            data_dma,
        }),
    }))
}

fn allocate_queue(device: *mut c_void, qid: u16, entry_count: u16) -> IoResult<NvmeQueue> {
    let sq_bytes = entry_count as usize * core::mem::size_of::<NvmeSubmission>();
    let cq_bytes = entry_count as usize * core::mem::size_of::<NvmeCompletion>();
    let sq_cpu = alloc_dma_buffer(device, sq_bytes)?.cast::<NvmeSubmission>();
    let cq_cpu = alloc_dma_buffer(device, cq_bytes)?.cast::<NvmeCompletion>();
    Ok(NvmeQueue {
        qid,
        sq_cpu,
        sq_dma: crate::memory::paging::kernel_virtual_to_physical_addr(sq_cpu as u64),
        cq_cpu,
        cq_dma: crate::memory::paging::kernel_virtual_to_physical_addr(cq_cpu as u64),
        entry_count,
        sq_tail: 0,
        cq_head: 0,
        cq_phase: 1,
        next_cid: 1,
    })
}

fn alloc_dma_buffer(device: *mut c_void, size: usize) -> IoResult<*mut u8> {
    let mut dma_handle = 0_u64;
    let cpu = crate::driver::dma::alloc_coherent(device, size, &mut dma_handle).cast::<u8>();
    if cpu.is_null() {
        Err(DiskIoError::NotPresent)
    } else {
        Ok(cpu)
    }
}

fn identify_controller_model(
    controller: &NvmeController,
    admin: &mut NvmeQueue,
    identify_dma: u64,
    identify_cpu: *mut u8,
) -> IoResult<String> {
    unsafe {
        ptr::write_bytes(identify_cpu, 0, NVME_IDENTIFY_BYTES);
    }
    let cmd = NvmeSubmission {
        cdw0: build_cdw0(NVME_ADMIN_OP_IDENTIFY, admin.alloc_cid()),
        nsid: 0,
        rsvd2: 0,
        mptr: 0,
        prp1: identify_dma,
        prp2: 0,
        cdw10: NVME_IDENTIFY_CNS_CONTROLLER,
        cdw11: 0,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };
    submit_and_wait(controller, admin, cmd)?;
    let bytes = unsafe { core::slice::from_raw_parts(identify_cpu, NVME_IDENTIFY_BYTES) };
    Ok(trim_ascii(&bytes[24..64]))
}

fn configure_io_queues(
    controller: &NvmeController,
    admin: &mut NvmeQueue,
    io: &NvmeQueue,
) -> IoResult<()> {
    let set_features = NvmeSubmission {
        cdw0: build_cdw0(NVME_ADMIN_OP_SET_FEATURES, admin.alloc_cid()),
        nsid: 0,
        rsvd2: 0,
        mptr: 0,
        prp1: 0,
        prp2: 0,
        cdw10: NVME_FEATURE_NUM_QUEUES,
        cdw11: 0,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };
    submit_and_wait(controller, admin, set_features)?;

    let create_cq = NvmeSubmission {
        cdw0: build_cdw0(NVME_ADMIN_OP_CREATE_IO_CQ, admin.alloc_cid()),
        nsid: 0,
        rsvd2: 0,
        mptr: 0,
        prp1: io.cq_dma,
        prp2: 0,
        cdw10: ((io.entry_count as u32 - 1) << 16) | io.qid as u32,
        cdw11: 1,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };
    submit_and_wait(controller, admin, create_cq)?;

    let create_sq = NvmeSubmission {
        cdw0: build_cdw0(NVME_ADMIN_OP_CREATE_IO_SQ, admin.alloc_cid()),
        nsid: 0,
        rsvd2: 0,
        mptr: 0,
        prp1: io.sq_dma,
        prp2: 0,
        cdw10: ((io.entry_count as u32 - 1) << 16) | io.qid as u32,
        cdw11: ((io.qid as u32) << 16) | 1,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };
    submit_and_wait(controller, admin, create_sq)?;
    Ok(())
}

fn identify_namespace(
    controller: &NvmeController,
    admin: &mut NvmeQueue,
    identify_dma: u64,
    identify_cpu: *mut u8,
) -> IoResult<(u64, usize, u32)> {
    unsafe {
        ptr::write_bytes(identify_cpu, 0, NVME_IDENTIFY_BYTES);
    }
    let namespace_id = 1_u32;
    let cmd = NvmeSubmission {
        cdw0: build_cdw0(NVME_ADMIN_OP_IDENTIFY, admin.alloc_cid()),
        nsid: namespace_id,
        rsvd2: 0,
        mptr: 0,
        prp1: identify_dma,
        prp2: 0,
        cdw10: NVME_IDENTIFY_CNS_NAMESPACE,
        cdw11: 0,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };
    submit_and_wait(controller, admin, cmd)?;

    let bytes = unsafe { core::slice::from_raw_parts(identify_cpu, NVME_IDENTIFY_BYTES) };
    let block_count = le_u64(bytes, 0);
    if block_count == 0 {
        return Err(DiskIoError::NotPresent);
    }
    let nlbaf = bytes[25] as usize;
    let flbas = (bytes[26] & 0x0f) as usize;
    if flbas > nlbaf {
        return Err(DiskIoError::InvalidInput);
    }
    let lbaf = 128 + flbas * 4;
    let block_shift = bytes[lbaf + 2];
    let logical_block_size = 1usize
        .checked_shl(block_shift as u32)
        .ok_or(DiskIoError::InvalidInput)?;
    Ok((block_count, logical_block_size, namespace_id))
}

fn issue_nvm_command(
    controller: &NvmeController,
    io: &mut NvmeQueue,
    namespace_id: u32,
    opcode: u8,
    lba: u64,
    data_dma: u64,
    block_count: u16,
) -> IoResult<()> {
    if block_count == 0 {
        return Err(DiskIoError::InvalidInput);
    }
    let cmd = NvmeSubmission {
        cdw0: build_cdw0(opcode, io.alloc_cid()),
        nsid: namespace_id,
        rsvd2: 0,
        mptr: 0,
        prp1: data_dma,
        prp2: 0,
        cdw10: lba as u32,
        cdw11: (lba >> 32) as u32,
        cdw12: (block_count - 1) as u32,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };
    submit_and_wait(controller, io, cmd).map(|_| ())
}

fn submit_and_wait(
    controller: &NvmeController,
    queue: &mut NvmeQueue,
    cmd: NvmeSubmission,
) -> IoResult<u32> {
    let sq_index = queue.sq_tail as usize;
    unsafe {
        ptr::write_volatile(queue.sq_cpu.add(sq_index), cmd);
    }
    queue.sq_tail = (queue.sq_tail + 1) % queue.entry_count;
    fence(Ordering::SeqCst);
    controller.ring_sq(queue);

    if !wait_until(|| {
        let entry = unsafe { ptr::read_volatile(queue.cq_cpu.add(queue.cq_head as usize)) };
        (entry.status & 1) == queue.cq_phase
    }) {
        return Err(DiskIoError::Timeout);
    }

    let entry = unsafe { ptr::read_volatile(queue.cq_cpu.add(queue.cq_head as usize)) };
    if entry.cid != command_cid(&cmd) {
        return Err(DiskIoError::InvalidInput);
    }
    if ((entry.status >> 1) & 0xff) != 0 {
        return Err(DiskIoError::InvalidInput);
    }

    queue.cq_head += 1;
    if queue.cq_head == queue.entry_count {
        queue.cq_head = 0;
        queue.cq_phase ^= 1;
    }
    controller.ring_cq(queue);
    Ok(entry.result)
}

fn build_cdw0(opcode: u8, cid: u16) -> u32 {
    (opcode as u32) | ((cid as u32) << 16)
}

fn command_cid(cmd: &NvmeSubmission) -> u16 {
    (cmd.cdw0 >> 16) as u16
}

fn trim_ascii(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    while end != 0 && matches!(bytes[end - 1], b' ' | 0) {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..NVME_WAIT_SPINS {
        if predicate() {
            return true;
        }
        spin_loop();
    }
    false
}
