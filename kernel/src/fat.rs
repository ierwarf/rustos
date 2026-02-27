use alloc::vec;
use alloc::vec::Vec;
use core::cmp::min;
use core::hint::spin_loop;

use fatfs::{IoBase, IoError, Read, Seek, SeekFrom, Write};
use x86_64::instructions::{interrupts, port::Port};

pub const FAT_SECTOR_SIZE: usize = 512;
pub type IoResult<T> = core::result::Result<T, DiskIoError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiskIoError {
    Interrupted,
    UnexpectedEof,
    WriteZero,
    InvalidInput,
    Timeout,
    DeviceFault,
    NotPresent,
}

impl IoError for DiskIoError {
    fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
    }

    fn new_unexpected_eof_error() -> Self {
        Self::UnexpectedEof
    }

    fn new_write_zero_error() -> Self {
        Self::WriteZero
    }
}

/// FAT adapter target: provide raw sector read/write for your storage backend.
pub trait BlockDevice {
    fn sector_count(&self) -> u64;
    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()>;
    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()>;
    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

/// Simple in-memory block device for FAT testing/development.
pub struct MemBlockDevice {
    data: Vec<u8>,
}

impl MemBlockDevice {
    pub fn new_zeroed(sectors: u64) -> Self {
        let bytes = sectors.saturating_mul(FAT_SECTOR_SIZE as u64) as usize;
        Self {
            data: vec![0; bytes],
        }
    }

    pub fn from_bytes(data: Vec<u8>) -> IoResult<Self> {
        if data.len() % FAT_SECTOR_SIZE != 0 {
            return Err(DiskIoError::InvalidInput);
        }
        Ok(Self { data })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    fn sector_bounds(&self, lba: u64) -> IoResult<(usize, usize)> {
        let start = (lba as usize)
            .checked_mul(FAT_SECTOR_SIZE)
            .ok_or(DiskIoError::InvalidInput)?;
        let end = start
            .checked_add(FAT_SECTOR_SIZE)
            .ok_or(DiskIoError::InvalidInput)?;
        if end > self.data.len() {
            return Err(DiskIoError::InvalidInput);
        }
        Ok((start, end))
    }
}

impl BlockDevice for MemBlockDevice {
    fn sector_count(&self) -> u64 {
        (self.data.len() / FAT_SECTOR_SIZE) as u64
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        let (start, end) = self.sector_bounds(lba)?;
        out.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        let (start, end) = self.sector_bounds(lba)?;
        self.data[start..end].copy_from_slice(input);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtaDrive {
    Master,
    Slave,
}

impl AtaDrive {
    fn select_bits(self) -> u8 {
        match self {
            Self::Master => 0,
            Self::Slave => 1 << 4,
        }
    }
}

/// Legacy ATA PIO controller (IDE compatibility mode).
///
/// This works only when firmware/chipset exposes a legacy ATA channel
/// (for example some QEMU setups). Many modern laptops with NVMe-only
/// storage will not expose this path.
pub struct AtaPioDevice {
    io_base: u16,
    ctrl_base: u16,
    drive: AtaDrive,
    total_sectors: u64,
    lba48: bool,
}

impl AtaPioDevice {
    const REG_DATA: u16 = 0;
    const REG_SECTOR_COUNT: u16 = 2;
    const REG_LBA0: u16 = 3;
    const REG_LBA1: u16 = 4;
    const REG_LBA2: u16 = 5;
    const REG_DRIVE_HEAD: u16 = 6;
    const REG_STATUS_COMMAND: u16 = 7;

    const STATUS_ERR: u8 = 1 << 0;
    const STATUS_DRQ: u8 = 1 << 3;
    const STATUS_DF: u8 = 1 << 5;
    const STATUS_BSY: u8 = 1 << 7;

    const CMD_IDENTIFY: u8 = 0xEC;
    const CMD_READ_SECTORS: u8 = 0x20;
    const CMD_WRITE_SECTORS: u8 = 0x30;
    const CMD_READ_SECTORS_EXT: u8 = 0x24;
    const CMD_WRITE_SECTORS_EXT: u8 = 0x34;
    const CMD_FLUSH_CACHE: u8 = 0xE7;
    const CMD_FLUSH_CACHE_EXT: u8 = 0xEA;

    const WAIT_SPINS: usize = 2_000_000;

    pub fn primary_master() -> IoResult<Self> {
        Self::new(0x1F0, 0x3F6, AtaDrive::Master)
    }

    pub fn primary_slave() -> IoResult<Self> {
        Self::new(0x1F0, 0x3F6, AtaDrive::Slave)
    }

    pub fn new(io_base: u16, ctrl_base: u16, drive: AtaDrive) -> IoResult<Self> {
        let mut dev = Self {
            io_base,
            ctrl_base,
            drive,
            total_sectors: 0,
            lba48: false,
        };
        dev.identify()?;
        Ok(dev)
    }

    fn read_u8(&self, reg: u16) -> u8 {
        unsafe {
            let mut port: Port<u8> = Port::new(self.io_base + reg);
            port.read()
        }
    }

    fn write_u8(&self, reg: u16, value: u8) {
        unsafe {
            let mut port: Port<u8> = Port::new(self.io_base + reg);
            port.write(value);
        }
    }

    fn read_data_u16(&self) -> u16 {
        unsafe {
            let mut port: Port<u16> = Port::new(self.io_base + Self::REG_DATA);
            port.read()
        }
    }

    fn write_data_u16(&self, value: u16) {
        unsafe {
            let mut port: Port<u16> = Port::new(self.io_base + Self::REG_DATA);
            port.write(value);
        }
    }

    fn read_alt_status(&self) -> u8 {
        unsafe {
            let mut port: Port<u8> = Port::new(self.ctrl_base);
            port.read()
        }
    }

    fn status_400ns_delay(&self) {
        let _ = self.read_alt_status();
        let _ = self.read_alt_status();
        let _ = self.read_alt_status();
        let _ = self.read_alt_status();
    }

    fn wait_not_busy(&self) -> IoResult<u8> {
        for _ in 0..Self::WAIT_SPINS {
            let status = self.read_u8(Self::REG_STATUS_COMMAND);
            if status & Self::STATUS_BSY == 0 {
                if status & Self::STATUS_DF != 0 {
                    return Err(DiskIoError::DeviceFault);
                }
                if status & Self::STATUS_ERR != 0 {
                    return Err(DiskIoError::InvalidInput);
                }
                return Ok(status);
            }
            spin_loop();
        }
        Err(DiskIoError::Timeout)
    }

    fn wait_drq(&self) -> IoResult<()> {
        for _ in 0..Self::WAIT_SPINS {
            let status = self.read_u8(Self::REG_STATUS_COMMAND);
            if status & Self::STATUS_BSY != 0 {
                spin_loop();
                continue;
            }
            if status & Self::STATUS_DF != 0 {
                return Err(DiskIoError::DeviceFault);
            }
            if status & Self::STATUS_ERR != 0 {
                return Err(DiskIoError::InvalidInput);
            }
            if status & Self::STATUS_DRQ != 0 {
                return Ok(());
            }
            spin_loop();
        }
        Err(DiskIoError::Timeout)
    }

    fn select_drive_base(&self) {
        self.write_u8(Self::REG_DRIVE_HEAD, 0xE0 | self.drive.select_bits());
        self.status_400ns_delay();
    }

    fn select_drive_lba28(&self, lba: u64) -> IoResult<()> {
        if lba > 0x0FFF_FFFF {
            return Err(DiskIoError::InvalidInput);
        }
        self.write_u8(
            Self::REG_DRIVE_HEAD,
            0xE0 | self.drive.select_bits() | (((lba >> 24) as u8) & 0x0F),
        );
        self.status_400ns_delay();
        Ok(())
    }

    fn identify(&mut self) -> IoResult<()> {
        interrupts::without_interrupts(|| {
            self.select_drive_base();
            self.write_u8(Self::REG_SECTOR_COUNT, 0);
            self.write_u8(Self::REG_LBA0, 0);
            self.write_u8(Self::REG_LBA1, 0);
            self.write_u8(Self::REG_LBA2, 0);
            self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_IDENTIFY);

            if self.read_u8(Self::REG_STATUS_COMMAND) == 0 {
                return Err(DiskIoError::NotPresent);
            }

            self.wait_not_busy()?;
            // ATAPI/SATA packet devices can show non-zero here in IDENTIFY.
            if self.read_u8(Self::REG_LBA1) != 0 || self.read_u8(Self::REG_LBA2) != 0 {
                return Err(DiskIoError::InvalidInput);
            }
            self.wait_drq()?;

            let mut id = [0u16; 256];
            for word in &mut id {
                *word = self.read_data_u16();
            }

            let lba28 = ((id[61] as u32) << 16) | (id[60] as u32);
            let lba48_supported = (id[83] & (1 << 10)) != 0;
            let lba48 = ((id[103] as u64) << 48)
                | ((id[102] as u64) << 32)
                | ((id[101] as u64) << 16)
                | (id[100] as u64);

            self.lba48 = lba48_supported && lba48 > 0;
            self.total_sectors = if self.lba48 { lba48 } else { lba28 as u64 };
            if self.total_sectors == 0 {
                return Err(DiskIoError::NotPresent);
            }
            Ok(())
        })
    }

    fn read_sector_lba28(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_lba28(lba)?;
        self.write_u8(Self::REG_SECTOR_COUNT, 1);
        self.write_u8(Self::REG_LBA0, (lba & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 8) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 16) & 0xFF) as u8);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_READ_SECTORS);
        self.wait_drq()?;

        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let v = self.read_data_u16();
            out[i * 2] = (v & 0x00FF) as u8;
            out[i * 2 + 1] = (v >> 8) as u8;
        }
        Ok(())
    }

    fn write_sector_lba28(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_lba28(lba)?;
        self.write_u8(Self::REG_SECTOR_COUNT, 1);
        self.write_u8(Self::REG_LBA0, (lba & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 8) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 16) & 0xFF) as u8);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_WRITE_SECTORS);
        self.wait_drq()?;

        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let lo = input[i * 2] as u16;
            let hi = (input[i * 2 + 1] as u16) << 8;
            self.write_data_u16(lo | hi);
        }
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_FLUSH_CACHE);
        let _ = self.wait_not_busy()?;
        Ok(())
    }

    fn program_lba48_regs(&mut self, lba: u64) {
        self.write_u8(Self::REG_SECTOR_COUNT, 0);
        self.write_u8(Self::REG_LBA0, ((lba >> 24) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 32) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 40) & 0xFF) as u8);

        self.write_u8(Self::REG_SECTOR_COUNT, 1);
        self.write_u8(Self::REG_LBA0, (lba & 0xFF) as u8);
        self.write_u8(Self::REG_LBA1, ((lba >> 8) & 0xFF) as u8);
        self.write_u8(Self::REG_LBA2, ((lba >> 16) & 0xFF) as u8);
    }

    fn read_sector_lba48(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_base();
        self.program_lba48_regs(lba);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_READ_SECTORS_EXT);
        self.wait_drq()?;
        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let v = self.read_data_u16();
            out[i * 2] = (v & 0x00FF) as u8;
            out[i * 2 + 1] = (v >> 8) as u8;
        }
        Ok(())
    }

    fn write_sector_lba48(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        self.select_drive_base();
        self.program_lba48_regs(lba);
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_WRITE_SECTORS_EXT);
        self.wait_drq()?;
        for i in 0..(FAT_SECTOR_SIZE / 2) {
            let lo = input[i * 2] as u16;
            let hi = (input[i * 2 + 1] as u16) << 8;
            self.write_data_u16(lo | hi);
        }
        self.write_u8(Self::REG_STATUS_COMMAND, Self::CMD_FLUSH_CACHE_EXT);
        let _ = self.wait_not_busy()?;
        Ok(())
    }
}

impl BlockDevice for AtaPioDevice {
    fn sector_count(&self) -> u64 {
        self.total_sectors
    }

    fn read_sector(&mut self, lba: u64, out: &mut [u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        if lba >= self.total_sectors {
            return Err(DiskIoError::InvalidInput);
        }

        interrupts::without_interrupts(|| {
            if self.lba48 {
                self.read_sector_lba48(lba, out)
            } else {
                self.read_sector_lba28(lba, out)
            }
        })
    }

    fn write_sector(&mut self, lba: u64, input: &[u8; FAT_SECTOR_SIZE]) -> IoResult<()> {
        if lba >= self.total_sectors {
            return Err(DiskIoError::InvalidInput);
        }

        interrupts::without_interrupts(|| {
            if self.lba48 {
                self.write_sector_lba48(lba, input)
            } else {
                self.write_sector_lba28(lba, input)
            }
        })
    }

    fn flush(&mut self) -> IoResult<()> {
        interrupts::without_interrupts(|| {
            self.write_u8(
                Self::REG_STATUS_COMMAND,
                if self.lba48 {
                    Self::CMD_FLUSH_CACHE_EXT
                } else {
                    Self::CMD_FLUSH_CACHE
                },
            );
            let _ = self.wait_not_busy()?;
            Ok(())
        })
    }
}

/// Wraps a sector device and exposes byte-stream IO required by `fatfs`.
pub struct FatDisk<D: BlockDevice> {
    dev: D,
    pos: u64,
    scratch: [u8; FAT_SECTOR_SIZE],
}

impl<D: BlockDevice> FatDisk<D> {
    pub fn new(dev: D) -> Self {
        Self {
            dev,
            pos: 0,
            scratch: [0; FAT_SECTOR_SIZE],
        }
    }

    fn bytes_len(&self) -> u64 {
        self.dev
            .sector_count()
            .saturating_mul(FAT_SECTOR_SIZE as u64)
    }

    fn ensure_in_range(&self, pos: u64) -> IoResult<()> {
        if pos <= self.bytes_len() {
            Ok(())
        } else {
            Err(DiskIoError::InvalidInput)
        }
    }
}

impl<D: BlockDevice> IoBase for FatDisk<D> {
    type Error = DiskIoError;
}

impl<D: BlockDevice> Read for FatDisk<D> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let disk_len = self.bytes_len();
        if self.pos >= disk_len {
            return Ok(0);
        }

        let max_read = min(buf.len() as u64, disk_len - self.pos) as usize;
        let mut done = 0usize;

        while done < max_read {
            let lba = self.pos / FAT_SECTOR_SIZE as u64;
            let off = (self.pos as usize) % FAT_SECTOR_SIZE;

            self.dev.read_sector(lba, &mut self.scratch)?;

            let n = min(FAT_SECTOR_SIZE - off, max_read - done);
            buf[done..done + n].copy_from_slice(&self.scratch[off..off + n]);

            self.pos += n as u64;
            done += n;
        }

        Ok(done)
    }
}

impl<D: BlockDevice> Write for FatDisk<D> {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let disk_len = self.bytes_len();
        if self.pos >= disk_len {
            return Ok(0);
        }

        let max_write = min(buf.len() as u64, disk_len - self.pos) as usize;
        let mut done = 0usize;

        while done < max_write {
            let lba = self.pos / FAT_SECTOR_SIZE as u64;
            let off = (self.pos as usize) % FAT_SECTOR_SIZE;
            let remaining = max_write - done;
            let n = min(FAT_SECTOR_SIZE - off, remaining);

            if off == 0 && n == FAT_SECTOR_SIZE {
                self.scratch
                    .copy_from_slice(&buf[done..done + FAT_SECTOR_SIZE]);
            } else {
                self.dev.read_sector(lba, &mut self.scratch)?;
                self.scratch[off..off + n].copy_from_slice(&buf[done..done + n]);
            }

            self.dev.write_sector(lba, &self.scratch)?;
            self.pos += n as u64;
            done += n;
        }

        Ok(done)
    }

    fn flush(&mut self) -> IoResult<()> {
        self.dev.flush()
    }
}

impl<D: BlockDevice> Seek for FatDisk<D> {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        let len = self.bytes_len() as i128;
        let cur = self.pos as i128;

        let next = match pos {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(delta) => len
                .checked_add(delta as i128)
                .ok_or(DiskIoError::InvalidInput)?,
            SeekFrom::Current(delta) => cur
                .checked_add(delta as i128)
                .ok_or(DiskIoError::InvalidInput)?,
        };

        if next < 0 {
            return Err(DiskIoError::InvalidInput);
        }

        let next_u64 = next as u64;
        self.ensure_in_range(next_u64)?;
        self.pos = next_u64;
        Ok(self.pos)
    }
}

/// Minimal FAT usage example with an explicit device:
/// mount -> create file -> write bytes -> unmount.
pub fn mount_and_create_hello_file_with_device<D: BlockDevice>(
    dev: D,
) -> core::result::Result<(), fatfs::Error<DiskIoError>> {
    let disk = FatDisk::new(dev);
    let fs = fatfs::FileSystem::new(disk, fatfs::FsOptions::new())?;
    {
        let root = fs.root_dir();
        let mut file = root.create_file("HELLO.TXT")?;
        file.truncate()?;
        file.write_all(b"hello from rustos\n")?;
        file.flush()?;
    }
    fs.unmount()?;
    Ok(())
}

/// Minimal FAT usage example using real ATA legacy channels.
///
/// Tries `primary master` first, then `primary slave`.
pub fn mount_and_create_hello_file() -> core::result::Result<(), fatfs::Error<DiskIoError>> {
    let dev = AtaPioDevice::primary_master().or_else(|_| AtaPioDevice::primary_slave());
    match dev {
        Ok(dev) => mount_and_create_hello_file_with_device(dev),
        Err(_) => Err(fatfs::Error::Io(DiskIoError::NotPresent)),
    }
}
