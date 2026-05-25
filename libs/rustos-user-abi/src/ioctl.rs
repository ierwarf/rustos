pub const NRBITS: u64 = 8;
pub const TYPEBITS: u64 = 8;
pub const SIZEBITS: u64 = 14;

pub const NRSHIFT: u64 = 0;
pub const TYPESHIFT: u64 = NRSHIFT + NRBITS;
pub const SIZESHIFT: u64 = TYPESHIFT + TYPEBITS;
pub const DIRSHIFT: u64 = SIZESHIFT + SIZEBITS;

pub const NONE: u64 = 0;
pub const WRITE: u64 = 1;
pub const READ: u64 = 2;

pub const fn ioc(dir: u64, type_: u8, nr: u8, size: u64) -> u64 {
    (dir << DIRSHIFT)
        | ((type_ as u64) << TYPESHIFT)
        | ((nr as u64) << NRSHIFT)
        | (size << SIZESHIFT)
}

pub const fn ior<T>(type_: u8, nr: u8) -> u64 {
    ioc(READ, type_, nr, core::mem::size_of::<T>() as u64)
}

pub const fn iow<T>(type_: u8, nr: u8) -> u64 {
    ioc(WRITE, type_, nr, core::mem::size_of::<T>() as u64)
}

pub const fn iowr<T>(type_: u8, nr: u8) -> u64 {
    ioc(READ | WRITE, type_, nr, core::mem::size_of::<T>() as u64)
}
