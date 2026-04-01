use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::ptr;
use core::slice;

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
const PCI_SUBCLASS_SATA: u8 = 0x06;
const PCI_PROG_IF_AHCI: u8 = 0x01;
const AHCI_BAR_INDEX: usize = 5;

const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0c;
const HBA_PORT_BASE: usize = 0x100;
const HBA_PORT_STRIDE: usize = 0x80;

const PORT_CLB: usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB: usize = 0x08;
const PORT_FBU: usize = 0x0c;
const PORT_IS: usize = 0x10;
const PORT_IE: usize = 0x14;
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_SIG: usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SERR: usize = 0x30;
const PORT_SACT: usize = 0x34;
const PORT_CI: usize = 0x38;

const GHC_AE: u32 = 1 << 31;
const CAP_S64A: u32 = 1 << 31;
const PORT_CMD_ST: u32 = 1 << 0;
const PORT_CMD_FRE: u32 = 1 << 4;
const PORT_CMD_FR: u32 = 1 << 14;
const PORT_CMD_CR: u32 = 1 << 15;
const PORT_TFD_ERR: u32 = 1 << 0;
const PORT_TFD_DRQ: u32 = 1 << 3;
const PORT_TFD_BSY: u32 = 1 << 7;
const PORT_SSTS_DET_MASK: u32 = 0x0f;
const PORT_SSTS_IPM_MASK: u32 = 0x0f00;
const PORT_SSTS_DET_PRESENT: u32 = 0x03;
const SATA_SIGNATURE_ATA: u32 = 0x0000_0101;

const FIS_TYPE_REG_H2D: u8 = 0x27;
const ATA_CMD_IDENTIFY_DEVICE: u8 = 0xec;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xea;

const COMMAND_FIS_DWORDS: u16 = 5;
const COMMAND_LIST_BYTES: usize = 1024;
const RECEIVED_FIS_BYTES: usize = 256;
const COMMAND_TABLE_BYTES: usize = 4096;
const DMA_BUFFER_BYTES: usize = 4096;
const COMMAND_SLOT: u32 = 0;
const AHCI_WAIT_SPINS: usize = 1_000_000;
const LOGICAL_BLOCK_SIZE: usize = 512;

#[repr(C)]
struct AhciCommandHeader {
    flags: u16,
    prdtl: u16,
    prdbc: u32,
    ctba: u32,
    ctbau: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct AhciPrdtEntry {
    dba: u32,
    dbau: u32,
    reserved: u32,
    dbc: u32,
}

#[repr(C)]
struct AhciCommandTable {
    cfis: [u8; 64],
    acmd: [u8; 16],
    reserved: [u8; 48],
    prdt: [AhciPrdtEntry; 1],
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

struct AhciController {
    mmio_base: usize,
    mmio_len: usize,
    supports_64bit_dma: bool,
}

unsafe impl Send for AhciController {}
unsafe impl Sync for AhciController {}

struct AhciPortRuntime {
    command_list_cpu: *mut u8,
    command_list_dma: u64,
    _received_fis_cpu: *mut u8,
    received_fis_dma: u64,
    command_table_cpu: *mut AhciCommandTable,
    command_table_dma: u64,
    dma_buffer_cpu: *mut u8,
    dma_buffer_dma: u64,
}

pub(crate) struct AhciBlockDevice {
    controller: Arc<AhciController>,
    port: u8,
    sector_count: u64,
    #[allow(dead_code)]
    model: String,
    runtime: AhciPortRuntime,
}

unsafe impl Send for AhciBlockDevice {}

pub(crate) fn probe_devices() -> Vec<AhciBlockDevice> {
    let mut devices = Vec::new();
    for pci in visit_pci_devices() {
        if pci.class_code() != PCI_CLASS_MASS_STORAGE
            || pci.subclass() != PCI_SUBCLASS_SATA
            || pci.prog_if() != PCI_PROG_IF_AHCI
        {
            continue;
        }
        match probe_controller(pci) {
            Ok(mut found) => devices.append(&mut found),
            Err(err) => crate::debug::println!(
                "prekernel storage: ahci controller {:02x}:{:02x}.{} skipped: {:?}",
                pci.bus,
                pci.device,
                pci.function,
                err
            ),
        }
    }
    devices
}

impl BlockDevice for AhciBlockDevice {
    fn logical_block_size(&self) -> usize {
        LOGICAL_BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.sector_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        if out.is_empty() || out.len() % LOGICAL_BLOCK_SIZE != 0 {
            return Err(StorageError::InvalidInput);
        }
        for (index, chunk) in out.chunks_exact_mut(LOGICAL_BLOCK_SIZE).enumerate() {
            let lba = lba
                .checked_add(index as u64)
                .ok_or(StorageError::InvalidInput)?;
            if lba >= self.sector_count {
                return Err(StorageError::InvalidInput);
            }
            self.execute_dma_command(ATA_CMD_READ_DMA_EXT, lba, false)?;
            unsafe {
                ptr::copy_nonoverlapping(
                    self.runtime.dma_buffer_cpu.cast_const(),
                    chunk.as_mut_ptr(),
                    LOGICAL_BLOCK_SIZE,
                );
            }
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        if input.is_empty() || input.len() % LOGICAL_BLOCK_SIZE != 0 {
            return Err(StorageError::InvalidInput);
        }
        for (index, chunk) in input.chunks_exact(LOGICAL_BLOCK_SIZE).enumerate() {
            let lba = lba
                .checked_add(index as u64)
                .ok_or(StorageError::InvalidInput)?;
            if lba >= self.sector_count {
                return Err(StorageError::InvalidInput);
            }
            unsafe {
                ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    self.runtime.dma_buffer_cpu,
                    LOGICAL_BLOCK_SIZE,
                );
            }
            self.execute_dma_command(ATA_CMD_WRITE_DMA_EXT, lba, true)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        self.execute_non_data_command(ATA_CMD_FLUSH_CACHE_EXT)
    }
}

impl PciDevice {
    fn is_present(self) -> bool {
        self.vendor_id() != 0xffff
    }

    fn vendor_id(self) -> u16 {
        self.read_u16(0x00)
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

impl AhciController {
    fn read_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::read_volatile((self.mmio_base + offset) as *const u32) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        debug_assert!(offset + 4 <= self.mmio_len);
        unsafe { ptr::write_volatile((self.mmio_base + offset) as *mut u32, value) };
    }

    fn read_port_u32(&self, port: u8, reg: usize) -> u32 {
        self.read_u32(HBA_PORT_BASE + port as usize * HBA_PORT_STRIDE + reg)
    }

    fn write_port_u32(&self, port: u8, reg: usize, value: u32) {
        self.write_u32(HBA_PORT_BASE + port as usize * HBA_PORT_STRIDE + reg, value);
    }

    fn enable_ahci_mode(&self) {
        let ghc = self.read_u32(HBA_GHC);
        self.write_u32(HBA_GHC, ghc | GHC_AE);
    }
}

impl AhciBlockDevice {
    fn execute_non_data_command(&mut self, command: u8) -> IoResult<()> {
        self.prepare_command(command, 0, false, 0)?;
        self.issue_and_wait()
    }

    fn execute_dma_command(&mut self, command: u8, lba: u64, is_write: bool) -> IoResult<()> {
        self.prepare_command(command, lba, is_write, LOGICAL_BLOCK_SIZE)?;
        self.issue_and_wait()
    }

    fn prepare_command(
        &mut self,
        command: u8,
        lba: u64,
        is_write: bool,
        byte_count: usize,
    ) -> IoResult<()> {
        prepare_port_command(
            self.controller.as_ref(),
            self.port,
            &mut self.runtime,
            command,
            lba,
            is_write,
            byte_count,
        )
    }

    fn issue_and_wait(&self) -> IoResult<()> {
        issue_port_command(self.controller.as_ref(), self.port)
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

fn probe_controller(pci: PciDevice) -> Result<Vec<AhciBlockDevice>, StorageError> {
    let abar = pci
        .resource(AHCI_BAR_INDEX)
        .filter(|resource| !resource.is_io && resource.size >= 0x200)
        .ok_or(StorageError::NotPresent)?;

    pci.enable_memory_bus_master();
    let controller = Arc::new(AhciController {
        mmio_base: abar.start as usize,
        mmio_len: abar.size as usize,
        supports_64bit_dma: false,
    });
    controller.enable_ahci_mode();
    let supports_64bit_dma = (controller.read_u32(HBA_CAP) & CAP_S64A) != 0;
    let controller = Arc::new(AhciController {
        mmio_base: abar.start as usize,
        mmio_len: abar.size as usize,
        supports_64bit_dma,
    });
    controller.enable_ahci_mode();

    let mut devices = Vec::new();
    let ports = controller.read_u32(HBA_PI);
    for port in 0..32 {
        if (ports & (1 << port)) == 0 {
            continue;
        }
        if let Some(device) = probe_port(controller.clone(), port as u8)? {
            devices.push(device);
        }
    }
    Ok(devices)
}

fn probe_port(
    controller: Arc<AhciController>,
    port: u8,
) -> Result<Option<AhciBlockDevice>, StorageError> {
    if !port_has_sata_device(&controller, port) {
        return Ok(None);
    }

    stop_command_engine(&controller, port)?;
    let mut runtime = allocate_port_runtime(controller.as_ref())?;
    program_port_dma(&controller, port, &runtime);
    start_command_engine(&controller, port)?;

    let (sector_count, model) = identify_port(&controller, port, &mut runtime)?;
    crate::debug::println!(
        "prekernel storage: ahci port {} online sectors={} model={}",
        port,
        sector_count,
        model
    );
    Ok(Some(AhciBlockDevice {
        controller,
        port,
        sector_count,
        model,
        runtime,
    }))
}

fn port_has_sata_device(controller: &AhciController, port: u8) -> bool {
    let ssts = controller.read_port_u32(port, PORT_SSTS);
    let det = ssts & PORT_SSTS_DET_MASK;
    let ipm = (ssts & PORT_SSTS_IPM_MASK) >> 8;
    if det != PORT_SSTS_DET_PRESENT || ipm == 0 {
        return false;
    }
    controller.read_port_u32(port, PORT_SIG) == SATA_SIGNATURE_ATA
}

fn stop_command_engine(controller: &AhciController, port: u8) -> IoResult<()> {
    let cmd = controller.read_port_u32(port, PORT_CMD) & !(PORT_CMD_ST | PORT_CMD_FRE);
    controller.write_port_u32(port, PORT_CMD, cmd);
    if wait_until(|| {
        let cmd = controller.read_port_u32(port, PORT_CMD);
        (cmd & (PORT_CMD_CR | PORT_CMD_FR)) == 0
    }) {
        Ok(())
    } else {
        Err(StorageError::Timeout)
    }
}

fn start_command_engine(controller: &AhciController, port: u8) -> IoResult<()> {
    if !wait_until(|| (controller.read_port_u32(port, PORT_CMD) & PORT_CMD_CR) == 0) {
        return Err(StorageError::Timeout);
    }
    let cmd = controller.read_port_u32(port, PORT_CMD) | PORT_CMD_FRE | PORT_CMD_ST;
    controller.write_port_u32(port, PORT_CMD, cmd);
    Ok(())
}

fn allocate_port_runtime(controller: &AhciController) -> IoResult<AhciPortRuntime> {
    let (command_list_cpu, command_list_dma) =
        alloc_dma(controller.supports_64bit_dma, COMMAND_LIST_BYTES)?;
    let (received_fis_cpu, received_fis_dma) =
        alloc_dma(controller.supports_64bit_dma, RECEIVED_FIS_BYTES)?;
    let (command_table_cpu, command_table_dma) =
        alloc_dma(controller.supports_64bit_dma, COMMAND_TABLE_BYTES)?;
    let (dma_buffer_cpu, dma_buffer_dma) =
        alloc_dma(controller.supports_64bit_dma, DMA_BUFFER_BYTES)?;

    Ok(AhciPortRuntime {
        command_list_cpu,
        command_list_dma,
        _received_fis_cpu: received_fis_cpu,
        received_fis_dma,
        command_table_cpu: command_table_cpu.cast(),
        command_table_dma,
        dma_buffer_cpu,
        dma_buffer_dma,
    })
}

fn alloc_dma(supports_64bit_dma: bool, size: usize) -> IoResult<(*mut u8, u64)> {
    let layout = Layout::from_size_align(size, 4096).map_err(|_| StorageError::InvalidInput)?;
    let cpu_ptr = unsafe { alloc_zeroed(layout) };
    if cpu_ptr.is_null() {
        return Err(StorageError::NotPresent);
    }
    let dma_addr = cpu_ptr as u64;
    if !supports_64bit_dma
        && dma_addr.saturating_add(size.saturating_sub(1) as u64) > u32::MAX as u64
    {
        unsafe {
            dealloc(cpu_ptr, layout);
        }
        return Err(StorageError::NotPresent);
    }
    Ok((cpu_ptr, dma_addr))
}

fn program_port_dma(controller: &AhciController, port: u8, runtime: &AhciPortRuntime) {
    controller.write_port_u32(port, PORT_CLB, runtime.command_list_dma as u32);
    controller.write_port_u32(port, PORT_CLBU, (runtime.command_list_dma >> 32) as u32);
    controller.write_port_u32(port, PORT_FB, runtime.received_fis_dma as u32);
    controller.write_port_u32(port, PORT_FBU, (runtime.received_fis_dma >> 32) as u32);
    controller.write_port_u32(port, PORT_IE, 0);
    controller.write_port_u32(port, PORT_IS, u32::MAX);
    controller.write_port_u32(port, PORT_SERR, u32::MAX);
}

fn identify_port(
    controller: &Arc<AhciController>,
    port: u8,
    runtime: &mut AhciPortRuntime,
) -> IoResult<(u64, String)> {
    prepare_port_command(
        controller.as_ref(),
        port,
        runtime,
        ATA_CMD_IDENTIFY_DEVICE,
        0,
        false,
        LOGICAL_BLOCK_SIZE,
    )?;
    issue_port_command(controller.as_ref(), port)?;

    let identify = unsafe { slice::from_raw_parts(runtime.dma_buffer_cpu.cast::<u16>(), 256) };
    let lba28 = ((identify[61] as u32) << 16) | identify[60] as u32;
    let lba48_supported = (identify[83] & (1 << 10)) != 0;
    let lba48 = ((identify[103] as u64) << 48)
        | ((identify[102] as u64) << 32)
        | ((identify[101] as u64) << 16)
        | identify[100] as u64;
    let sector_count = if lba48_supported && lba48 > 0 {
        lba48
    } else {
        lba28 as u64
    };
    if sector_count == 0 {
        return Err(StorageError::NotPresent);
    }
    Ok((sector_count, decode_identify_string(identify, 27, 20)))
}

fn decode_identify_string(words: &[u16], start: usize, word_count: usize) -> String {
    let mut bytes = Vec::with_capacity(word_count * 2);
    for word in &words[start..start + word_count] {
        bytes.push((word >> 8) as u8);
        bytes.push((word & 0xff) as u8);
    }
    while matches!(bytes.last(), Some(b' ' | 0)) {
        bytes.pop();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn prepare_port_command(
    controller: &AhciController,
    port: u8,
    runtime: &mut AhciPortRuntime,
    command: u8,
    lba: u64,
    is_write: bool,
    byte_count: usize,
) -> IoResult<()> {
    if !wait_until(|| {
        let tfd = controller.read_port_u32(port, PORT_TFD);
        (tfd & (PORT_TFD_BSY | PORT_TFD_DRQ)) == 0
    }) {
        return Err(StorageError::Timeout);
    }

    unsafe {
        ptr::write_bytes(runtime.command_list_cpu, 0, COMMAND_LIST_BYTES);
        ptr::write_bytes(
            runtime.command_table_cpu.cast::<u8>(),
            0,
            COMMAND_TABLE_BYTES,
        );
    }

    let header = runtime.command_list_cpu.cast::<AhciCommandHeader>();
    let fis_len_flags = COMMAND_FIS_DWORDS | if is_write { 1 << 6 } else { 0 };
    unsafe {
        (*header).flags = fis_len_flags;
        (*header).prdtl = if byte_count == 0 { 0 } else { 1 };
        (*header).prdbc = 0;
        (*header).ctba = runtime.command_table_dma as u32;
        (*header).ctbau = (runtime.command_table_dma >> 32) as u32;
    }

    let table = runtime.command_table_cpu;
    unsafe {
        (*table).cfis[0] = FIS_TYPE_REG_H2D;
        (*table).cfis[1] = 1 << 7;
        (*table).cfis[2] = command;
        (*table).cfis[4] = (lba & 0xff) as u8;
        (*table).cfis[5] = ((lba >> 8) & 0xff) as u8;
        (*table).cfis[6] = ((lba >> 16) & 0xff) as u8;
        (*table).cfis[7] = 1 << 6;
        (*table).cfis[8] = ((lba >> 24) & 0xff) as u8;
        (*table).cfis[9] = ((lba >> 32) & 0xff) as u8;
        (*table).cfis[10] = ((lba >> 40) & 0xff) as u8;
        (*table).cfis[12] = if byte_count == 0 { 0 } else { 1 };
        if byte_count != 0 {
            (*table).prdt[0].dba = runtime.dma_buffer_dma as u32;
            (*table).prdt[0].dbau = (runtime.dma_buffer_dma >> 32) as u32;
            (*table).prdt[0].reserved = 0;
            (*table).prdt[0].dbc = ((byte_count - 1) as u32) | (1 << 31);
        }
    }
    Ok(())
}

fn issue_port_command(controller: &AhciController, port: u8) -> IoResult<()> {
    let bit = 1_u32 << COMMAND_SLOT;
    if !wait_until(|| {
        let active =
            controller.read_port_u32(port, PORT_SACT) | controller.read_port_u32(port, PORT_CI);
        active & bit == 0
    }) {
        return Err(StorageError::Timeout);
    }

    controller.write_port_u32(port, PORT_CI, bit);

    if !wait_until(|| {
        let ci = controller.read_port_u32(port, PORT_CI);
        let tfd = controller.read_port_u32(port, PORT_TFD);
        (ci & bit) == 0 && (tfd & (PORT_TFD_BSY | PORT_TFD_DRQ)) == 0
    }) {
        return Err(StorageError::Timeout);
    }

    let tfd = controller.read_port_u32(port, PORT_TFD);
    if (tfd & PORT_TFD_ERR) != 0 {
        return Err(StorageError::InvalidInput);
    }
    Ok(())
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..AHCI_WAIT_SPINS {
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
