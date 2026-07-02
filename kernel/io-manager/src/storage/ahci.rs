// RING3-MIGRATION-REFERENCE START: storaged/driverd should own the non-.ko AHCI
// service-driver once a ring3 service-driver host can perform boot-volume block
// I/O through leased MMIO/DMA/IRQ resources before rootd/vfsd need storage.
// Ring0 keeps this built-in AHCI path as early-boot/fallback privileged
// transport substrate until that host exists.
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use storage_core::BlockDevice as SharedBlockDevice;

use crate::arch::pci::PciDevice;
use crate::storage::block::{BlockDeviceOps, BlockTransportKind};
use crate::storage::fat::{DiskIoError, IoResult};
use crate::sync::KernelWaitLock;

const PCI_CLASS_MASS_STORAGE: u8 = 0x01;
const PCI_SUBCLASS_SATA: u8 = 0x06;
const PCI_PROG_IF_AHCI: u8 = 0x01;
const AHCI_BAR_INDEX: usize = 5;

const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0c;
const HBA_VS: usize = 0x10;

const GHC_AE: u32 = 1 << 31;
const CAP_S64A: u32 = 1 << 31;

const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
const PORT_CLB: usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB: usize = 0x08;
const PORT_FBU: usize = 0x0c;
const PORT_IS: usize = 0x10;
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_SIG: usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SERR: usize = 0x30;
const PORT_CI: usize = 0x38;

const PORT_CMD_ST: u32 = 1 << 0;
const PORT_CMD_SUD: u32 = 1 << 1;
const PORT_CMD_POD: u32 = 1 << 2;
const PORT_CMD_FRE: u32 = 1 << 4;
const PORT_CMD_FR: u32 = 1 << 14;
const PORT_CMD_CR: u32 = 1 << 15;
const PORT_IS_TFES: u32 = 1 << 30;
const PORT_TFD_ERR: u32 = 1 << 0;
const PORT_TFD_DRQ: u32 = 1 << 3;
const PORT_TFD_BSY: u32 = 1 << 7;
const SATA_SIG_ATA: u32 = 0x0000_0101;

const FIS_TYPE_REG_H2D: u8 = 0x27;
const ATA_CMD_IDENTIFY: u8 = 0xec;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_DEVICE_LBA: u8 = 1 << 6;

const AHCI_CMD_LIST_BYTES: usize = 1024;
const AHCI_FIS_BYTES: usize = 256;
const AHCI_CMD_TABLE_BYTES: usize = 256;
const AHCI_IDENTIFY_BYTES: usize = 512;
const AHCI_DATA_BUFFER_BYTES: usize = 64 * 1024;
const AHCI_SECTOR_BYTES: usize = 512;
const AHCI_WAIT_SPINS: usize = 5_000_000;

struct AhciController {
    mmio_base: usize,
    mmio_len: usize,
    dma_key: *mut c_void,
}

unsafe impl Send for AhciController {}
unsafe impl Sync for AhciController {}

struct AhciRuntime {
    cmd_list_cpu: *mut u8,
    cmd_list_dma: u64,
    fis_cpu: *mut u8,
    fis_dma: u64,
    cmd_table_cpu: *mut u8,
    cmd_table_dma: u64,
    identify_cpu: *mut u8,
    identify_dma: u64,
    data_cpu: *mut u8,
    data_dma: u64,
}

unsafe impl Send for AhciRuntime {}

// Built-in AHCI is an early-boot/fallback path. Post-bootstrap storage policy,
// port admission, and long-term I/O ownership still belong in storaged.
struct AhciBlockDevice {
    controller: Arc<AhciController>,
    port: usize,
    sector_count: u64,
    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    model: String,
    runtime: KernelWaitLock<AhciRuntime>,
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
        512
    }

    fn block_count(&self) -> u64 {
        self.sector_count
    }

    fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> IoResult<()> {
        if out.len() % AHCI_SECTOR_BYTES != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let mut offset = 0usize;
        while offset < out.len() {
            let chunk_len = core::cmp::min(AHCI_DATA_BUFFER_BYTES, out.len() - offset);
            let sectors = chunk_len / AHCI_SECTOR_BYTES;
            self.issue_data_command(
                ATA_CMD_READ_DMA_EXT,
                lba + (offset / AHCI_SECTOR_BYTES) as u64,
                sectors as u16,
                &mut out[offset..offset + chunk_len],
            )?;
            offset += chunk_len;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, input: &[u8]) -> IoResult<()> {
        if input.len() % AHCI_SECTOR_BYTES != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        let mut offset = 0usize;
        while offset < input.len() {
            let chunk_len = core::cmp::min(AHCI_DATA_BUFFER_BYTES, input.len() - offset);
            let sectors = chunk_len / AHCI_SECTOR_BYTES;
            let mut chunk = input[offset..offset + chunk_len].to_vec();
            self.issue_data_command(
                ATA_CMD_WRITE_DMA_EXT,
                lba + (offset / AHCI_SECTOR_BYTES) as u64,
                sectors as u16,
                &mut chunk,
            )?;
            offset += chunk_len;
        }
        Ok(())
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
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

    fn port_offset(port: usize, offset: usize) -> usize {
        PORT_BASE + port * PORT_STRIDE + offset
    }

    fn read_port_u32(&self, port: usize, offset: usize) -> u32 {
        self.read_u32(Self::port_offset(port, offset))
    }

    fn write_port_u32(&self, port: usize, offset: usize, value: u32) {
        self.write_u32(Self::port_offset(port, offset), value);
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

    let ports = controller.implemented_ports();
    crate::debug::println!(
        "ahci: controller {:02x}:{:02x}.{} version={:#x} ports={:#x}",
        pci.bus,
        pci.device,
        pci.function,
        controller.version(),
        ports
    );

    let mut devices = Vec::new();
    for port in 0..32 {
        if (ports & (1 << port)) == 0 {
            continue;
        }
        match probe_port(controller.clone(), port) {
            Ok(Some(device)) => devices.push(Box::new(device) as Box<dyn BlockDeviceOps>),
            Ok(None) => {}
            Err(_error) => {
                crate::debug::println!("ahci: port {} skipped: {:?}", port, _error);
            }
        }
    }

    Ok(devices)
}

impl AhciBlockDevice {
    fn issue_data_command(
        &self,
        command: u8,
        lba: u64,
        sectors: u16,
        buffer: &mut [u8],
    ) -> IoResult<()> {
        if sectors == 0 || buffer.len() != usize::from(sectors) * AHCI_SECTOR_BYTES {
            return Err(DiskIoError::InvalidInput);
        }
        let write = command == ATA_CMD_WRITE_DMA_EXT;
        let mut runtime = self.runtime.lock();
        unsafe {
            if write {
                ptr::copy_nonoverlapping(buffer.as_ptr(), runtime.data_cpu, buffer.len());
            } else {
                ptr::write_bytes(runtime.data_cpu, 0, buffer.len());
            }
        }
        let data_dma = runtime.data_dma;
        issue_ata_command(
            self.controller.as_ref(),
            self.port,
            &mut runtime,
            command,
            lba,
            sectors,
            data_dma,
            buffer.len(),
            write,
        )?;
        if !write {
            unsafe {
                ptr::copy_nonoverlapping(runtime.data_cpu, buffer.as_mut_ptr(), buffer.len());
            }
        }
        Ok(())
    }
}

fn probe_port(controller: Arc<AhciController>, port: usize) -> IoResult<Option<AhciBlockDevice>> {
    let ssts = controller.read_port_u32(port, PORT_SSTS);
    let det = ssts & 0x0f;
    let ipm = (ssts >> 8) & 0x0f;
    if det != 3 || ipm != 1 {
        return Ok(None);
    }
    let sig = controller.read_port_u32(port, PORT_SIG);
    if sig != SATA_SIG_ATA {
        return Ok(None);
    }

    let mut runtime = AhciRuntime {
        cmd_list_cpu: alloc_dma_buffer(controller.dma_key, AHCI_CMD_LIST_BYTES)?,
        cmd_list_dma: 0,
        fis_cpu: alloc_dma_buffer(controller.dma_key, AHCI_FIS_BYTES)?,
        fis_dma: 0,
        cmd_table_cpu: alloc_dma_buffer(controller.dma_key, AHCI_CMD_TABLE_BYTES)?,
        cmd_table_dma: 0,
        identify_cpu: alloc_dma_buffer(controller.dma_key, AHCI_IDENTIFY_BYTES)?,
        identify_dma: 0,
        data_cpu: alloc_dma_buffer(controller.dma_key, AHCI_DATA_BUFFER_BYTES)?,
        data_dma: 0,
    };
    runtime.cmd_list_dma =
        crate::memory::paging::kernel_virtual_to_physical_addr(runtime.cmd_list_cpu as u64);
    runtime.fis_dma =
        crate::memory::paging::kernel_virtual_to_physical_addr(runtime.fis_cpu as u64);
    runtime.cmd_table_dma =
        crate::memory::paging::kernel_virtual_to_physical_addr(runtime.cmd_table_cpu as u64);
    runtime.identify_dma =
        crate::memory::paging::kernel_virtual_to_physical_addr(runtime.identify_cpu as u64);
    runtime.data_dma =
        crate::memory::paging::kernel_virtual_to_physical_addr(runtime.data_cpu as u64);

    configure_port(controller.as_ref(), port, &runtime)?;
    unsafe {
        ptr::write_bytes(runtime.identify_cpu, 0, AHCI_IDENTIFY_BYTES);
    }
    let identify_dma = runtime.identify_dma;
    issue_ata_command(
        controller.as_ref(),
        port,
        &mut runtime,
        ATA_CMD_IDENTIFY,
        0,
        0,
        identify_dma,
        AHCI_IDENTIFY_BYTES,
        false,
    )?;
    let identify =
        unsafe { core::slice::from_raw_parts(runtime.identify_cpu, AHCI_IDENTIFY_BYTES) };
    let sector_count = identify_sector_count(identify).ok_or(DiskIoError::Unsupported)?;
    let model = identify_model(identify);
    crate::debug::println!(
        "ahci: port {} blocks={} block_size={} model={}",
        port,
        sector_count,
        AHCI_SECTOR_BYTES,
        model
    );

    Ok(Some(AhciBlockDevice {
        controller,
        port,
        sector_count,
        model,
        runtime: KernelWaitLock::new(runtime),
    }))
}

fn configure_port(controller: &AhciController, port: usize, runtime: &AhciRuntime) -> IoResult<()> {
    let mut cmd = controller.read_port_u32(port, PORT_CMD);
    cmd &= !PORT_CMD_ST;
    controller.write_port_u32(port, PORT_CMD, cmd);
    if !wait_until(|| (controller.read_port_u32(port, PORT_CMD) & PORT_CMD_CR) == 0) {
        return Err(DiskIoError::Timeout);
    }
    cmd &= !PORT_CMD_FRE;
    controller.write_port_u32(port, PORT_CMD, cmd);
    if !wait_until(|| (controller.read_port_u32(port, PORT_CMD) & PORT_CMD_FR) == 0) {
        return Err(DiskIoError::Timeout);
    }

    controller.write_port_u32(port, PORT_CLB, runtime.cmd_list_dma as u32);
    controller.write_port_u32(port, PORT_CLBU, (runtime.cmd_list_dma >> 32) as u32);
    controller.write_port_u32(port, PORT_FB, runtime.fis_dma as u32);
    controller.write_port_u32(port, PORT_FBU, (runtime.fis_dma >> 32) as u32);
    controller.write_port_u32(port, PORT_SERR, u32::MAX);
    controller.write_port_u32(port, PORT_IS, u32::MAX);

    unsafe {
        ptr::write_bytes(runtime.cmd_list_cpu, 0, AHCI_CMD_LIST_BYTES);
        ptr::write_bytes(runtime.fis_cpu, 0, AHCI_FIS_BYTES);
        ptr::write_bytes(runtime.cmd_table_cpu, 0, AHCI_CMD_TABLE_BYTES);
    }

    cmd = controller.read_port_u32(port, PORT_CMD)
        | PORT_CMD_FRE
        | PORT_CMD_ST
        | PORT_CMD_SUD
        | PORT_CMD_POD;
    controller.write_port_u32(port, PORT_CMD, cmd);
    Ok(())
}

fn issue_ata_command(
    controller: &AhciController,
    port: usize,
    runtime: &mut AhciRuntime,
    command: u8,
    lba: u64,
    sectors: u16,
    data_dma: u64,
    byte_len: usize,
    write: bool,
) -> IoResult<()> {
    if byte_len == 0 || byte_len > AHCI_DATA_BUFFER_BYTES {
        return Err(DiskIoError::InvalidInput);
    }
    if byte_len > 4 * 1024 * 1024 {
        return Err(DiskIoError::Unsupported);
    }
    if !wait_until(|| {
        (controller.read_port_u32(port, PORT_TFD) & (PORT_TFD_BSY | PORT_TFD_DRQ)) == 0
    }) {
        return Err(DiskIoError::Timeout);
    }

    unsafe {
        ptr::write_bytes(runtime.cmd_table_cpu, 0, AHCI_CMD_TABLE_BYTES);
        ptr::write_bytes(runtime.cmd_list_cpu, 0, 32);

        let header = runtime.cmd_list_cpu;
        write_u16(header, 0, 5 | if write { 1 << 6 } else { 0 });
        write_u16(header, 2, 1);
        write_u32(header, 4, 0);
        write_u32(header, 8, runtime.cmd_table_dma as u32);
        write_u32(header, 12, (runtime.cmd_table_dma >> 32) as u32);

        let cfis = runtime.cmd_table_cpu;
        ptr::write_volatile(cfis.add(0), FIS_TYPE_REG_H2D);
        ptr::write_volatile(cfis.add(1), 1 << 7);
        ptr::write_volatile(cfis.add(2), command);
        ptr::write_volatile(cfis.add(4), lba as u8);
        ptr::write_volatile(cfis.add(5), (lba >> 8) as u8);
        ptr::write_volatile(cfis.add(6), (lba >> 16) as u8);
        ptr::write_volatile(cfis.add(7), ATA_DEVICE_LBA);
        ptr::write_volatile(cfis.add(8), (lba >> 24) as u8);
        ptr::write_volatile(cfis.add(9), (lba >> 32) as u8);
        ptr::write_volatile(cfis.add(10), (lba >> 40) as u8);
        ptr::write_volatile(cfis.add(12), sectors as u8);
        ptr::write_volatile(cfis.add(13), (sectors >> 8) as u8);

        let prdt = runtime.cmd_table_cpu.add(0x80);
        write_u32(prdt, 0, data_dma as u32);
        write_u32(prdt, 4, (data_dma >> 32) as u32);
        write_u32(prdt, 8, 0);
        write_u32(prdt, 12, ((byte_len as u32 - 1) & 0x003f_ffff) | (1 << 31));
    }

    controller.write_port_u32(port, PORT_SERR, u32::MAX);
    controller.write_port_u32(port, PORT_IS, u32::MAX);
    fence(Ordering::SeqCst);
    controller.write_port_u32(port, PORT_CI, 1);
    if !wait_until(|| (controller.read_port_u32(port, PORT_CI) & 1) == 0) {
        return Err(DiskIoError::Timeout);
    }
    fence(Ordering::SeqCst);

    let is = controller.read_port_u32(port, PORT_IS);
    let tfd = controller.read_port_u32(port, PORT_TFD);
    if (is & PORT_IS_TFES) != 0 || (tfd & PORT_TFD_ERR) != 0 {
        return Err(DiskIoError::DeviceFault);
    }
    Ok(())
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

fn identify_sector_count(identify: &[u8]) -> Option<u64> {
    if identify.len() < AHCI_IDENTIFY_BYTES {
        return None;
    }
    let word83 = le_word(identify, 83);
    if (word83 & (1 << 10)) != 0 {
        let mut value = 0_u64;
        for index in 0..4 {
            value |= (le_word(identify, 100 + index) as u64) << (index * 16);
        }
        if value != 0 {
            return Some(value);
        }
    }
    let value = (le_word(identify, 60) as u64) | ((le_word(identify, 61) as u64) << 16);
    (value != 0).then_some(value)
}

fn identify_model(identify: &[u8]) -> String {
    let mut bytes = [0_u8; 40];
    for word in 0..20 {
        let offset = (27 + word) * 2;
        bytes[word * 2] = identify.get(offset + 1).copied().unwrap_or(b' ');
        bytes[word * 2 + 1] = identify.get(offset).copied().unwrap_or(b' ');
    }
    let start = bytes.iter().position(|byte| *byte != b' ').unwrap_or(0);
    let end = bytes
        .iter()
        .rposition(|byte| *byte != b' ')
        .map(|index| index + 1)
        .unwrap_or(start);
    core::str::from_utf8(&bytes[start..end])
        .unwrap_or("unknown")
        .into()
}

fn le_word(bytes: &[u8], word: usize) -> u16 {
    let offset = word * 2;
    u16::from_le_bytes([
        bytes.get(offset).copied().unwrap_or(0),
        bytes.get(offset + 1).copied().unwrap_or(0),
    ])
}

unsafe fn write_u16(base: *mut u8, offset: usize, value: u16) {
    unsafe { ptr::write_volatile(base.add(offset).cast::<u16>(), value) };
}

unsafe fn write_u32(base: *mut u8, offset: usize, value: u32) {
    unsafe { ptr::write_volatile(base.add(offset).cast::<u32>(), value) };
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..AHCI_WAIT_SPINS {
        if condition() {
            return true;
        }
        spin_loop();
    }
    false
}
// RING3-MIGRATION-REFERENCE END: storaged/driverd-owned non-.ko AHCI service-driver.
