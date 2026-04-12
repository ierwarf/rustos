use alloc::alloc::{Layout, alloc_zeroed};
use alloc::string::String;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use storage_core::{BlockDevice, IoResult, StorageError};
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS_PORT: u16 = 0x0cf8;
const CONFIG_DATA_PORT: u16 = 0x0cfc;

const COMMAND_OFFSET: u8 = 0x04;
const HEADER_TYPE_OFFSET: u8 = 0x0e;
const CLASS_CODE_OFFSET: u8 = 0x0b;
const SUBCLASS_OFFSET: u8 = 0x0a;
const PROG_IF_OFFSET: u8 = 0x09;
const BAR0_OFFSET: u8 = 0x10;

const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;
const HEADER_TYPE_MULTIFUNCTION: u8 = 1 << 7;
const PCI_STD_NUM_BARS: usize = 6;
const PCI_BAR_IO_SPACE: u32 = 1 << 0;
const PCI_BAR_MEM_TYPE_MASK: u32 = 0x6;
const PCI_BAR_MEM_TYPE_64: u32 = 0x4;
const PCI_BAR_PREFETCH: u32 = 0x8;
const PCI_BAR_IO_ADDRESS_MASK: u32 = !0x3;
const PCI_BAR_MEM_ADDRESS_MASK: u32 = !0xf;

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

#[derive(Clone, Copy)]
struct PciResource {
    start: u64,
    size: u64,
    is_io: bool,
}

#[derive(Clone, Copy)]
struct PciDevice {
    bus: u8,
    device: u8,
    function: u8,
}

struct NvmeController {
    mmio_base: usize,
    mmio_len: usize,
    doorbell_stride: usize,
    #[allow(dead_code)]
    version: u32,
}

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

pub(crate) struct NvmeBlockDevice {
    controller: NvmeController,
    #[allow(dead_code)]
    admin: NvmeQueue,
    io: NvmeQueue,
    #[allow(dead_code)]
    identify_cpu: *mut u8,
    #[allow(dead_code)]
    identify_dma: u64,
    data_cpu: *mut u8,
    data_dma: u64,
    namespace_id: u32,
    block_count: u64,
    logical_block_size: usize,
    #[allow(dead_code)]
    model: String,
}

unsafe impl Send for NvmeBlockDevice {}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
pub(crate) fn probe_devices() -> Vec<NvmeBlockDevice> {
    let mut devices = Vec::new();
    let mut saw_qemu_nvme = false;
    for pci in visit_pci_devices() {
        let vendor = pci.vendor_id();
        let device = pci.device_id();
        let class_code = pci.class_code();
        let subclass = pci.subclass();
        let prog_if = pci.prog_if();
        let qemu_nvme = vendor == 0x1b36 && device == 0x0010;

        if class_code == PCI_CLASS_MASS_STORAGE || qemu_nvme {
            crate::debug::println!(
                "prekernel storage: pci {:02x}:{:02x}.{} vendor={:04x} device={:04x} class={:02x}/{:02x}/{:02x}",
                pci.bus,
                pci.device,
                pci.function,
                vendor,
                device,
                class_code,
                subclass,
                prog_if
            );
        }

        if !(class_code == PCI_CLASS_MASS_STORAGE
            && subclass == PCI_SUBCLASS_NVM
            && prog_if == PCI_PROG_IF_NVME)
            && !qemu_nvme
        {
            continue;
        }
        saw_qemu_nvme |= qemu_nvme;
        match probe_controller(pci) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) => {}
            Err(err) => crate::debug::println!(
                "prekernel storage: nvme controller {:02x}:{:02x}.{} skipped: {:?}",
                pci.bus,
                pci.device,
                pci.function,
                err
            ),
        }
    }
    if saw_qemu_nvme && devices.is_empty() {
        crate::debug::println!(
            "prekernel storage: QEMU NVMe controller was visible in PCI scan but probe failed"
        );
    }
    devices
}

impl BlockDevice for NvmeBlockDevice {
    fn logical_block_size(&self) -> usize {
        self.logical_block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        if out.is_empty() || out.len() % self.logical_block_size != 0 {
            return Err(StorageError::InvalidInput);
        }
        for (index, chunk) in out.chunks_exact_mut(self.logical_block_size).enumerate() {
            let block_lba = lba
                .checked_add(index as u64)
                .ok_or(StorageError::InvalidInput)?;
            if block_lba >= self.block_count {
                return Err(StorageError::InvalidInput);
            }
            self.read_block(block_lba, chunk)?;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        if input.is_empty() || input.len() % self.logical_block_size != 0 {
            return Err(StorageError::InvalidInput);
        }
        for (index, chunk) in input.chunks_exact(self.logical_block_size).enumerate() {
            let block_lba = lba
                .checked_add(index as u64)
                .ok_or(StorageError::InvalidInput)?;
            if block_lba >= self.block_count {
                return Err(StorageError::InvalidInput);
            }
            self.write_block(block_lba, chunk)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        let cmd = NvmeSubmission {
            cdw0: build_cdw0(NVME_NVM_OP_FLUSH, self.io.alloc_cid()),
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
        self.controller
            .submit_and_wait(&mut self.io, cmd)
            .map(|_| ())
    }
}

impl PciDevice {
    fn is_present(self) -> bool {
        self.vendor_id() != 0xffff
    }

    fn vendor_id(self) -> u16 {
        self.read_u16(0x00)
    }

    fn device_id(self) -> u16 {
        self.read_u16(0x02)
    }

    fn class_code(self) -> u8 {
        self.read_u8(CLASS_CODE_OFFSET)
    }

    fn subclass(self) -> u8 {
        self.read_u8(SUBCLASS_OFFSET)
    }

    fn prog_if(self) -> u8 {
        self.read_u8(PROG_IF_OFFSET)
    }

    fn read_u8(self, offset: u8) -> u8 {
        let shift = ((offset & 0x3) * 8) as u32;
        ((self.read_u32(offset & !0x3) >> shift) & 0xff) as u8
    }

    fn read_u16(self, offset: u8) -> u16 {
        let shift = ((offset & 0x2) * 8) as u32;
        ((self.read_u32(offset & !0x3) >> shift) & 0xffff) as u16
    }

    fn read_u32(self, offset: u8) -> u32 {
        unsafe {
            let mut address_port = Port::<u32>::new(CONFIG_ADDRESS_PORT);
            let mut data_port = Port::<u32>::new(CONFIG_DATA_PORT);
            address_port.write(config_address(self.bus, self.device, self.function, offset));
            data_port.read()
        }
    }

    fn write_u32(self, offset: u8, value: u32) {
        unsafe {
            let mut address_port = Port::<u32>::new(CONFIG_ADDRESS_PORT);
            let mut data_port = Port::<u32>::new(CONFIG_DATA_PORT);
            address_port.write(config_address(self.bus, self.device, self.function, offset));
            data_port.write(value);
        }
    }

    fn update_command_bits(self, set_bits: u16) {
        let command = self.read_u16(COMMAND_OFFSET) | set_bits;
        let aligned = COMMAND_OFFSET & !0x3;
        let current = self.read_u32(aligned);
        let shift = ((COMMAND_OFFSET & 0x2) * 8) as u32;
        let mask = !(0xffff_u32 << shift);
        self.write_u32(aligned, (current & mask) | ((command as u32) << shift));
    }

    fn enable_memory_bus_master(self) {
        self.update_command_bits(COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER);
    }

    fn resource(self, bar_index: usize) -> Option<PciResource> {
        if bar_index >= PCI_STD_NUM_BARS {
            return None;
        }
        let bar_offset = BAR0_OFFSET + (bar_index as u8) * 4;
        let original_low = self.read_u32(bar_offset);
        if original_low == 0 {
            return None;
        }
        self.write_u32(bar_offset, u32::MAX);
        let mask_low = self.read_u32(bar_offset);
        self.write_u32(bar_offset, original_low);

        if (original_low & PCI_BAR_IO_SPACE) != 0 {
            let base = (original_low & PCI_BAR_IO_ADDRESS_MASK) as u64;
            let size_mask = mask_low & PCI_BAR_IO_ADDRESS_MASK;
            if size_mask == 0 {
                return None;
            }
            let size = (!(size_mask as u64)).wrapping_add(1);
            return Some(PciResource {
                start: base,
                size,
                is_io: true,
            });
        }

        let is_64bit = (original_low & PCI_BAR_MEM_TYPE_MASK) == PCI_BAR_MEM_TYPE_64;
        let mut original_high = 0_u32;
        let mut mask_high = 0_u32;
        if is_64bit {
            original_high = self.read_u32(bar_offset + 4);
            self.write_u32(bar_offset + 4, u32::MAX);
            mask_high = self.read_u32(bar_offset + 4);
            self.write_u32(bar_offset + 4, original_high);
        }

        let base_low = original_low & PCI_BAR_MEM_ADDRESS_MASK;
        let size_mask_low = mask_low & PCI_BAR_MEM_ADDRESS_MASK;
        let base = if is_64bit {
            ((original_high as u64) << 32) | base_low as u64
        } else {
            base_low as u64
        };
        let size_mask = if is_64bit {
            ((mask_high as u64) << 32) | size_mask_low as u64
        } else {
            size_mask_low as u64
        };
        if size_mask == 0 {
            return None;
        }
        let size = (!size_mask).wrapping_add(1);
        let _prefetchable = (original_low & PCI_BAR_PREFETCH) != 0;
        Some(PciResource {
            start: base,
            size,
            is_io: false,
        })
    }
}

impl NvmeController {
    fn read_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::read_volatile((self.mmio_base + offset) as *const u32) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::write_volatile((self.mmio_base + offset) as *mut u32, value) };
    }

    fn write_u64(&self, offset: usize, value: u64) {
        debug_assert!(offset + 8 <= self.mmio_len);
        unsafe { ptr::write_volatile((self.mmio_base + offset) as *mut u64, value) };
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
                return Err(StorageError::Timeout);
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
            return Err(StorageError::Timeout);
        }
        Ok(())
    }

    fn submit_and_wait(&self, queue: &mut NvmeQueue, cmd: NvmeSubmission) -> IoResult<u32> {
        let sq_index = queue.sq_tail as usize;
        unsafe {
            ptr::write_volatile(queue.sq_cpu.add(sq_index), cmd);
        }
        queue.sq_tail = (queue.sq_tail + 1) % queue.entry_count;
        fence(Ordering::SeqCst);
        self.ring_sq(queue);

        if !wait_until(|| {
            let entry = unsafe { ptr::read_volatile(queue.cq_cpu.add(queue.cq_head as usize)) };
            (entry.status & 1) == queue.cq_phase
        }) {
            return Err(StorageError::Timeout);
        }

        let entry = unsafe { ptr::read_volatile(queue.cq_cpu.add(queue.cq_head as usize)) };
        if entry.cid != command_cid(&cmd) {
            return Err(StorageError::InvalidInput);
        }
        let status = entry.status;
        let status_code = (status >> 1) & 0xff;
        if status_code != 0 {
            return Err(StorageError::InvalidInput);
        }

        queue.cq_head += 1;
        if queue.cq_head == queue.entry_count {
            queue.cq_head = 0;
            queue.cq_phase ^= 1;
        }
        self.ring_cq(queue);
        Ok(entry.result)
    }
}

impl NvmeQueue {
    fn alloc_cid(&mut self) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cid
    }
}

impl NvmeBlockDevice {
    fn read_block(&mut self, lba: u64, buffer: &mut [u8]) -> IoResult<()> {
        if self.logical_block_size > NVME_DATA_BUFFER_BYTES
            || buffer.len() != self.logical_block_size
        {
            return Err(StorageError::Unsupported);
        }
        let cmd = NvmeSubmission {
            cdw0: build_cdw0(NVME_NVM_OP_READ, self.io.alloc_cid()),
            nsid: self.namespace_id,
            rsvd2: 0,
            mptr: 0,
            prp1: self.data_dma,
            prp2: 0,
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        self.controller.submit_and_wait(&mut self.io, cmd)?;
        unsafe {
            ptr::copy_nonoverlapping(self.data_cpu, buffer.as_mut_ptr(), buffer.len());
        }
        Ok(())
    }

    fn write_block(&mut self, lba: u64, buffer: &[u8]) -> IoResult<()> {
        if self.logical_block_size > NVME_DATA_BUFFER_BYTES
            || buffer.len() != self.logical_block_size
        {
            return Err(StorageError::Unsupported);
        }
        unsafe {
            ptr::copy_nonoverlapping(buffer.as_ptr(), self.data_cpu, buffer.len());
        }
        let cmd = NvmeSubmission {
            cdw0: build_cdw0(NVME_NVM_OP_WRITE, self.io.alloc_cid()),
            nsid: self.namespace_id,
            rsvd2: 0,
            mptr: 0,
            prp1: self.data_dma,
            prp2: 0,
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        self.controller.submit_and_wait(&mut self.io, cmd)?;
        Ok(())
    }
}

fn visit_pci_devices() -> Vec<PciDevice> {
    let mut devices = Vec::new();
    for bus in 0..=u8::MAX {
        for device in 0..32_u8 {
            let pci = PciDevice {
                bus,
                device,
                function: 0,
            };
            if !pci.is_present() {
                continue;
            }

            let functions = if (pci.read_u8(HEADER_TYPE_OFFSET) & HEADER_TYPE_MULTIFUNCTION) != 0 {
                8
            } else {
                1
            };
            for function in 0..functions {
                let pci = PciDevice {
                    bus,
                    device,
                    function,
                };
                if pci.is_present() {
                    devices.push(pci);
                }
            }
        }
    }
    devices
}

fn probe_controller(pci: PciDevice) -> Result<Option<NvmeBlockDevice>, StorageError> {
    let bar = pci
        .resource(NVME_BAR_INDEX)
        .filter(|resource| !resource.is_io && resource.size >= 0x2000)
        .ok_or(StorageError::NotPresent)?;

    pci.enable_memory_bus_master();

    let cap = unsafe { ptr::read_volatile(bar.start as *const u64) };
    let dstrd = ((cap >> 32) & 0xf) as usize;
    let mpsmin = ((cap >> 48) & 0xf) as u8;
    let _mpsmax = ((cap >> 52) & 0xf) as u8;
    if mpsmin > 0 {
        return Err(StorageError::Unsupported);
    }

    let controller = NvmeController {
        mmio_base: bar.start as usize,
        mmio_len: bar.size as usize,
        doorbell_stride: 4 << dstrd,
        version: unsafe { ptr::read_volatile((bar.start as usize + NVME_VS) as *const u32) },
    };

    let mut admin = allocate_queue(0, NVME_ADMIN_QUEUE_DEPTH)?;
    controller.configure_admin_queue(&admin)?;
    let identify_cpu = alloc_dma(NVME_IDENTIFY_BYTES)?;
    let identify_dma = identify_cpu as u64;
    let data_cpu = alloc_dma(NVME_DATA_BUFFER_BYTES)?;
    let data_dma = data_cpu as u64;

    let model = identify_controller_model(&controller, &mut admin, identify_dma, identify_cpu)?;
    let io = allocate_queue(1, NVME_IO_QUEUE_DEPTH)?;
    configure_io_queues(&controller, &mut admin, &io)?;
    let (block_count, logical_block_size, namespace_id) =
        identify_namespace(&controller, &mut admin, identify_dma, identify_cpu)?;

    crate::debug::println!(
        "prekernel storage: nvme controller {:02x}:{:02x}.{} version={:#x} blocks={} block_size={} model={}",
        pci.bus,
        pci.device,
        pci.function,
        controller.version,
        block_count,
        logical_block_size,
        model
    );

    Ok(Some(NvmeBlockDevice {
        controller,
        admin,
        io,
        identify_cpu,
        identify_dma,
        data_cpu,
        data_dma,
        namespace_id,
        block_count,
        logical_block_size,
        model,
    }))
}

fn allocate_queue(qid: u16, entry_count: u16) -> IoResult<NvmeQueue> {
    let sq_bytes = entry_count as usize * core::mem::size_of::<NvmeSubmission>();
    let cq_bytes = entry_count as usize * core::mem::size_of::<NvmeCompletion>();
    let sq_cpu = alloc_dma(sq_bytes)?.cast::<NvmeSubmission>();
    let cq_cpu = alloc_dma(cq_bytes)?.cast::<NvmeCompletion>();
    Ok(NvmeQueue {
        qid,
        sq_cpu,
        sq_dma: sq_cpu as u64,
        cq_cpu,
        cq_dma: cq_cpu as u64,
        entry_count,
        sq_tail: 0,
        cq_head: 0,
        cq_phase: 1,
        next_cid: 1,
    })
}

fn alloc_dma(size: usize) -> IoResult<*mut u8> {
    let layout =
        Layout::from_size_align(size.max(4096), 4096).map_err(|_| StorageError::InvalidInput)?;
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        Err(StorageError::NotPresent)
    } else {
        Ok(ptr)
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
    controller.submit_and_wait(admin, cmd)?;
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
    controller.submit_and_wait(admin, set_features)?;

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
    controller.submit_and_wait(admin, create_cq)?;

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
    controller.submit_and_wait(admin, create_sq)?;
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
    controller.submit_and_wait(admin, cmd)?;

    let bytes = unsafe { core::slice::from_raw_parts(identify_cpu, NVME_IDENTIFY_BYTES) };
    let block_count = le_u64(bytes, 0);
    if block_count == 0 {
        return Err(StorageError::NotPresent);
    }
    let nlbaf = bytes[25] as usize;
    let flbas = (bytes[26] & 0x0f) as usize;
    if flbas > nlbaf {
        return Err(StorageError::InvalidInput);
    }
    let lbaf = 128 + flbas * 4;
    let block_shift = bytes[lbaf + 2];
    let logical_block_size = 1usize
        .checked_shl(block_shift as u32)
        .ok_or(StorageError::InvalidInput)?;
    if logical_block_size > NVME_DATA_BUFFER_BYTES {
        return Err(StorageError::Unsupported);
    }
    Ok((block_count, logical_block_size, namespace_id))
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

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | (offset as u32 & 0xfc)
}
