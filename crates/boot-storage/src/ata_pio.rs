use core::hint::spin_loop;

use x86_64::instructions::{interrupts, port::Port};

use crate::{BlockDevice, DiskIoError, FAT_SECTOR_SIZE, IoResult};

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

    pub fn secondary_master() -> IoResult<Self> {
        Self::new(0x170, 0x376, AtaDrive::Master)
    }

    pub fn secondary_slave() -> IoResult<Self> {
        Self::new(0x170, 0x376, AtaDrive::Slave)
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
