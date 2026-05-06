use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::hint::spin_loop;
use core::ptr;
use core::slice;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use storage_core::BlockDevice as SharedBlockDevice;

use crate::arch::pci::PciDevice;
use crate::storage::block::{BlockDeviceOps, BlockTransportKind};
use crate::storage::fat::{DiskIoError, IoResult};

const PCI_CLASS_MASS_STORAGE: u8 = 0x01;
const PCI_SUBCLASS_SATA: u8 = 0x06;
const PCI_PROG_IF_AHCI: u8 = 0x01;
const AHCI_BAR_INDEX: usize = 5;

const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0c;
const HBA_VS: usize = 0x10;
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
const ATA_CMD_READ_DMA: u8 = 0xc8;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA: u8 = 0xca;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xea;

const COMMAND_FIS_DWORDS: u16 = 5;
const COMMAND_LIST_BYTES: usize = 1024;
const RECEIVED_FIS_BYTES: usize = 256;
const COMMAND_TABLE_BYTES: usize = 4096;
const DMA_BUFFER_BYTES: usize = 64 * 1024;
const COMMAND_SLOT: u32 = 0;
const AHCI_WAIT_SPINS: usize = 1_000_000;
const LOGICAL_BLOCK_SIZE: usize = 512;
const DEBUG_BOUNDARY_16M_LBA: u64 = (16 * 1024 * 1024) / LOGICAL_BLOCK_SIZE as u64;

static AHCI_16M_BOUNDARY_LOGGED: AtomicBool = AtomicBool::new(false);

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

struct AhciController {
    mmio_base: usize,
    mmio_len: usize,
    dma_key: *mut c_void,
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

unsafe impl Send for AhciPortRuntime {}

struct AhciBlockDevice {
    controller: Arc<AhciController>,
    port: u8,
    sector_count: u64,
    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    model: String,
    runtime: Mutex<AhciPortRuntime>,
}

unsafe impl Send for AhciBlockDevice {}
unsafe impl Sync for AhciBlockDevice {}

pub(crate) fn probe_devices() -> Vec<Box<dyn BlockDeviceOps>> {
    let mut devices = Vec::new();
    crate::arch::pci::visit_devices(|pci| {
        if pci.class_code() != PCI_CLASS_MASS_STORAGE
            || pci.subclass() != PCI_SUBCLASS_SATA
            || pci.prog_if() != PCI_PROG_IF_AHCI
        {
            return false;
        }
        match probe_controller(pci) {
            Ok(mut found) => devices.append(&mut found),
            Err(_error) => {
                crate::debug::println!(
                    "ahci: controller {:02x}:{:02x}.{} skipped: {:?}",
                    pci.bus,
                    pci.device,
                    pci.function,
                    _error
                );
            }
        }
        false
    });
    devices
}

impl SharedBlockDevice for AhciBlockDevice {
    fn logical_block_size(&self) -> usize {
        LOGICAL_BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.sector_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        if out.is_empty() || out.len() % LOGICAL_BLOCK_SIZE != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let mut runtime = self.runtime.lock();
        let mut offset = 0usize;
        while offset < out.len() {
            let block_offset = offset / LOGICAL_BLOCK_SIZE;
            let block_lba = lba
                .checked_add(block_offset as u64)
                .ok_or(DiskIoError::InvalidInput)?;
            let transfer_bytes = DMA_BUFFER_BYTES.min(out.len() - offset);
            let transfer_blocks = transfer_bytes / LOGICAL_BLOCK_SIZE;
            if transfer_blocks == 0 {
                return Err(DiskIoError::InvalidInput);
            }
            if block_lba
                .checked_add(transfer_blocks as u64)
                .ok_or(DiskIoError::InvalidInput)?
                > self.sector_count
            {
                return Err(DiskIoError::InvalidInput);
            }
            note_ahci_16m_boundary_once(block_lba, transfer_blocks as u64, false);
            let command = dma_command_for_range(
                ATA_CMD_READ_DMA,
                ATA_CMD_READ_DMA_EXT,
                block_lba,
                transfer_blocks,
            )?;
            let result = self.execute_dma_command(
                &mut runtime,
                command,
                block_lba,
                false,
                transfer_blocks * LOGICAL_BLOCK_SIZE,
            );
            if let Err(error) = result {
                crate::debug::println!(
                    "ahci: read failed model={} port={} lba={} error={:?}",
                    self.model,
                    self.port,
                    block_lba,
                    error
                );
                return Err(error);
            }
            unsafe {
                crate::arch::simd::copy_fast(
                    runtime.dma_buffer_cpu.cast_const(),
                    out[offset..offset + transfer_blocks * LOGICAL_BLOCK_SIZE].as_mut_ptr(),
                    transfer_blocks * LOGICAL_BLOCK_SIZE,
                );
            }
            offset += transfer_blocks * LOGICAL_BLOCK_SIZE;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        if input.is_empty() || input.len() % LOGICAL_BLOCK_SIZE != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let mut runtime = self.runtime.lock();
        let mut offset = 0usize;
        while offset < input.len() {
            let block_offset = offset / LOGICAL_BLOCK_SIZE;
            let block_lba = lba
                .checked_add(block_offset as u64)
                .ok_or(DiskIoError::InvalidInput)?;
            let transfer_bytes = DMA_BUFFER_BYTES.min(input.len() - offset);
            let transfer_blocks = transfer_bytes / LOGICAL_BLOCK_SIZE;
            if transfer_blocks == 0 {
                return Err(DiskIoError::InvalidInput);
            }
            if block_lba
                .checked_add(transfer_blocks as u64)
                .ok_or(DiskIoError::InvalidInput)?
                > self.sector_count
            {
                return Err(DiskIoError::InvalidInput);
            }
            note_ahci_16m_boundary_once(block_lba, transfer_blocks as u64, true);
            let byte_count = transfer_blocks * LOGICAL_BLOCK_SIZE;
            unsafe {
                crate::arch::simd::copy_fast(
                    input[offset..offset + byte_count].as_ptr(),
                    runtime.dma_buffer_cpu,
                    byte_count,
                );
            }
            let command = dma_command_for_range(
                ATA_CMD_WRITE_DMA,
                ATA_CMD_WRITE_DMA_EXT,
                block_lba,
                transfer_blocks,
            )?;
            self.execute_dma_command(&mut runtime, command, block_lba, true, byte_count)?;
            offset += byte_count;
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        let mut runtime = self.runtime.lock();
        self.execute_non_data_command(&mut runtime, ATA_CMD_FLUSH_CACHE_EXT)
    }
}

fn note_ahci_16m_boundary_once(lba: u64, blocks: u64, is_write: bool) {
    let end = lba.saturating_add(blocks);
    if lba > DEBUG_BOUNDARY_16M_LBA || end <= DEBUG_BOUNDARY_16M_LBA {
        return;
    }
    if AHCI_16M_BOUNDARY_LOGGED.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Storage,
        "ahci-16m-boundary",
        lba,
        (blocks << 1) | if is_write { 1 } else { 0 },
    );
    crate::debug::warn!(
        storage,
        "ahci: crossing 16MiB disk boundary lba={} blocks={} write={}",
        lba,
        blocks,
        is_write
    );
}

impl BlockDeviceOps for AhciBlockDevice {
    fn transport_kind(&self) -> BlockTransportKind {
        BlockTransportKind::Ahci
    }

    fn readonly(&self) -> bool {
        false
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

    fn port_offset(&self, port: u8, reg: usize) -> usize {
        HBA_PORT_BASE + port as usize * HBA_PORT_STRIDE + reg
    }

    fn read_port_u32(&self, port: u8, reg: usize) -> u32 {
        self.read_u32(self.port_offset(port, reg))
    }

    fn write_port_u32(&self, port: u8, reg: usize, value: u32) {
        self.write_u32(self.port_offset(port, reg), value);
    }

    fn enable_ahci_mode(&self) {
        let ghc = self.read_u32(HBA_GHC);
        self.write_u32(HBA_GHC, ghc | GHC_AE);
    }

    fn supports_64bit_dma(&self) -> bool {
        (self.read_u32(HBA_CAP) & CAP_S64A) != 0
    }

    fn implemented_ports(&self) -> u32 {
        self.read_u32(HBA_PI)
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    fn version(&self) -> u32 {
        self.read_u32(HBA_VS)
    }
}

impl AhciBlockDevice {
    fn execute_non_data_command(&self, runtime: &mut AhciPortRuntime, command: u8) -> IoResult<()> {
        self.prepare_command(runtime, command, 0, false, 0)?;
        self.issue_and_wait()
    }

    fn execute_dma_command(
        &self,
        runtime: &mut AhciPortRuntime,
        command: u8,
        lba: u64,
        is_write: bool,
        byte_count: usize,
    ) -> IoResult<()> {
        self.prepare_command(runtime, command, lba, is_write, byte_count)?;
        self.issue_and_wait()
    }

    fn prepare_command(
        &self,
        runtime: &mut AhciPortRuntime,
        command: u8,
        lba: u64,
        is_write: bool,
        byte_count: usize,
    ) -> IoResult<()> {
        prepare_port_command(
            self.controller.as_ref(),
            self.port,
            runtime,
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
fn probe_controller(pci: PciDevice) -> Result<Vec<Box<dyn BlockDeviceOps>>, DiskIoError> {
    let abar = pci
        .resource(AHCI_BAR_INDEX)
        .filter(|resource| !resource.is_io && resource.size >= 0x200)
        .ok_or(DiskIoError::NotPresent)?;

    pci.enable_memory_bus_master();
    let mmio = crate::driver::mmio::map(abar.start, abar.size as usize, false);
    if mmio.is_null() {
        return Err(DiskIoError::NotPresent);
    }

    let controller = Arc::new(AhciController {
        mmio_base: mmio as usize,
        mmio_len: abar.size as usize,
        dma_key: mmio,
    });
    let dma_mask = if controller.supports_64bit_dma() {
        u64::MAX
    } else {
        u32::MAX as u64
    };
    crate::driver::dma::set_mask_and_coherent(controller.dma_key, dma_mask);
    controller.enable_ahci_mode();

    let mut devices = Vec::new();
    let ports = controller.implemented_ports();
    crate::debug::println!(
        "ahci: controller {:02x}:{:02x}.{} version={:#x} ports={:#x}",
        pci.bus,
        pci.device,
        pci.function,
        controller.version(),
        ports
    );

    for port in 0..32 {
        if (ports & (1 << port)) == 0 {
            continue;
        }
        if let Some(device) = probe_port(controller.clone(), port as u8)? {
            devices.push(Box::new(device) as Box<dyn BlockDeviceOps>);
        }
    }

    Ok(devices)
}

fn probe_port(
    controller: Arc<AhciController>,
    port: u8,
) -> Result<Option<AhciBlockDevice>, DiskIoError> {
    if !port_has_sata_device(&controller, port) {
        return Ok(None);
    }

    stop_command_engine(&controller, port)?;
    let mut runtime = allocate_port_runtime(&controller)?;
    program_port_dma(&controller, port, &runtime);
    start_command_engine(&controller, port)?;

    let (sector_count, model) = identify_port(&controller, port, &mut runtime)?;
    crate::debug::println!(
        "ahci: port {} online sectors={} model={}",
        port,
        sector_count,
        model
    );

    Ok(Some(AhciBlockDevice {
        controller,
        port,
        sector_count,
        model,
        runtime: Mutex::new(runtime),
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
        Err(DiskIoError::Timeout)
    }
}

fn start_command_engine(controller: &AhciController, port: u8) -> IoResult<()> {
    if !wait_until(|| (controller.read_port_u32(port, PORT_CMD) & PORT_CMD_CR) == 0) {
        return Err(DiskIoError::Timeout);
    }
    let cmd = controller.read_port_u32(port, PORT_CMD) | PORT_CMD_FRE | PORT_CMD_ST;
    controller.write_port_u32(port, PORT_CMD, cmd);
    Ok(())
}

fn allocate_port_runtime(controller: &AhciController) -> IoResult<AhciPortRuntime> {
    let mut command_list_dma = 0_u64;
    let command_list_cpu = crate::driver::dma::alloc_coherent(
        controller.dma_key,
        COMMAND_LIST_BYTES,
        &mut command_list_dma,
    )
    .cast::<u8>();
    if command_list_cpu.is_null() {
        return Err(DiskIoError::NotPresent);
    }

    let mut received_fis_dma = 0_u64;
    let received_fis_cpu = crate::driver::dma::alloc_coherent(
        controller.dma_key,
        RECEIVED_FIS_BYTES,
        &mut received_fis_dma,
    )
    .cast::<u8>();
    if received_fis_cpu.is_null() {
        return Err(DiskIoError::NotPresent);
    }

    let mut command_table_dma = 0_u64;
    let command_table_cpu = crate::driver::dma::alloc_coherent(
        controller.dma_key,
        COMMAND_TABLE_BYTES,
        &mut command_table_dma,
    )
    .cast::<AhciCommandTable>();
    if command_table_cpu.is_null() {
        return Err(DiskIoError::NotPresent);
    }

    let mut dma_buffer_dma = 0_u64;
    let dma_buffer_cpu = crate::driver::dma::alloc_coherent(
        controller.dma_key,
        DMA_BUFFER_BYTES,
        &mut dma_buffer_dma,
    )
    .cast::<u8>();
    if dma_buffer_cpu.is_null() {
        return Err(DiskIoError::NotPresent);
    }

    Ok(AhciPortRuntime {
        command_list_cpu,
        command_list_dma,
        _received_fis_cpu: received_fis_cpu,
        received_fis_dma,
        command_table_cpu,
        command_table_dma,
        dma_buffer_cpu,
        dma_buffer_dma,
    })
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
        return Err(DiskIoError::NotPresent);
    }
    let model = decode_identify_string(identify, 27, 20);
    Ok((sector_count, model))
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
    if wait_until(|| {
        let tfd = controller.read_port_u32(port, PORT_TFD);
        (tfd & (PORT_TFD_BSY | PORT_TFD_DRQ)) == 0
    }) {
        controller.write_port_u32(port, PORT_IS, u32::MAX);
        controller.write_port_u32(port, PORT_SERR, u32::MAX);
    } else {
        crate::debug::println!(
            "ahci: prepare timeout port={} tfd={:#x} sact={:#x} ci={:#x} is={:#x} serr={:#x}",
            port,
            controller.read_port_u32(port, PORT_TFD),
            controller.read_port_u32(port, PORT_SACT),
            controller.read_port_u32(port, PORT_CI),
            controller.read_port_u32(port, PORT_IS),
            controller.read_port_u32(port, PORT_SERR)
        );
        return Err(DiskIoError::Timeout);
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
        (*table).cfis[3] = 0;
        (*table).cfis[4] = (lba & 0xff) as u8;
        (*table).cfis[5] = ((lba >> 8) & 0xff) as u8;
        (*table).cfis[6] = ((lba >> 16) & 0xff) as u8;
        if command == ATA_CMD_READ_DMA || command == ATA_CMD_WRITE_DMA {
            (*table).cfis[7] = 0xe0 | (((lba >> 24) & 0x0f) as u8);
            (*table).cfis[8] = 0;
            (*table).cfis[9] = 0;
            (*table).cfis[10] = 0;
        } else {
            (*table).cfis[7] = 1 << 6;
            (*table).cfis[8] = ((lba >> 24) & 0xff) as u8;
            (*table).cfis[9] = ((lba >> 32) & 0xff) as u8;
            (*table).cfis[10] = ((lba >> 40) & 0xff) as u8;
        }
        (*table).cfis[11] = 0;
        let sector_count = if byte_count == 0 {
            0
        } else {
            if byte_count % LOGICAL_BLOCK_SIZE != 0 || byte_count > DMA_BUFFER_BYTES {
                return Err(DiskIoError::InvalidInput);
            }
            byte_count / LOGICAL_BLOCK_SIZE
        };
        if sector_count > u16::MAX as usize {
            return Err(DiskIoError::InvalidInput);
        }
        (*table).cfis[12] = (sector_count & 0xff) as u8;
        (*table).cfis[13] = ((sector_count >> 8) & 0xff) as u8;

        if byte_count != 0 {
            (*table).prdt[0].dba = runtime.dma_buffer_dma as u32;
            (*table).prdt[0].dbau = (runtime.dma_buffer_dma >> 32) as u32;
            (*table).prdt[0].reserved = 0;
            (*table).prdt[0].dbc = ((byte_count - 1) as u32) | (1 << 31);
        }
    }
    Ok(())
}

fn dma_command_for_range(dma28: u8, dma48: u8, lba: u64, sector_count: usize) -> IoResult<u8> {
    let end = lba
        .checked_add(sector_count as u64)
        .ok_or(DiskIoError::InvalidInput)?;
    if sector_count <= 256 && end <= (1_u64 << 28) {
        Ok(dma28)
    } else {
        Ok(dma48)
    }
}

fn issue_port_command(controller: &AhciController, port: u8) -> IoResult<()> {
    let bit = 1_u32 << COMMAND_SLOT;
    if !wait_until(|| {
        let active =
            controller.read_port_u32(port, PORT_SACT) | controller.read_port_u32(port, PORT_CI);
        active & bit == 0
    }) {
        crate::debug::println!(
            "ahci: issue preflight timeout port={} sact={:#x} ci={:#x} tfd={:#x} is={:#x} serr={:#x}",
            port,
            controller.read_port_u32(port, PORT_SACT),
            controller.read_port_u32(port, PORT_CI),
            controller.read_port_u32(port, PORT_TFD),
            controller.read_port_u32(port, PORT_IS),
            controller.read_port_u32(port, PORT_SERR)
        );
        return Err(DiskIoError::Timeout);
    }

    controller.write_port_u32(port, PORT_CI, bit);

    if !wait_until(|| {
        let ci = controller.read_port_u32(port, PORT_CI);
        let tfd = controller.read_port_u32(port, PORT_TFD);
        (ci & bit) == 0 && (tfd & (PORT_TFD_BSY | PORT_TFD_DRQ)) == 0
    }) {
        crate::debug::println!(
            "ahci: issue completion timeout port={} ci={:#x} tfd={:#x} is={:#x} serr={:#x}",
            port,
            controller.read_port_u32(port, PORT_CI),
            controller.read_port_u32(port, PORT_TFD),
            controller.read_port_u32(port, PORT_IS),
            controller.read_port_u32(port, PORT_SERR)
        );
        return Err(DiskIoError::Timeout);
    }

    let tfd = controller.read_port_u32(port, PORT_TFD);
    if (tfd & PORT_TFD_ERR) != 0 {
        crate::debug::println!(
            "ahci: issue error port={} tfd={:#x} is={:#x} serr={:#x}",
            port,
            tfd,
            controller.read_port_u32(port, PORT_IS),
            controller.read_port_u32(port, PORT_SERR)
        );
        return Err(DiskIoError::InvalidInput);
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
