// RING3-MIGRATION-REFERENCE START: Linux .ko module loader is an explicit
// ring0 compatibility substrate. Driver load/admission policy is driverd-owned;
// ring0 keeps ELF relocation, module memory permissions, symbol binding, and
// init execution because Linux .ko execution is intentionally ring0.
use alloc::vec;
use alloc::vec::Vec;
use core::arch::asm;
use core::mem::size_of;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::KernelSpinLock as Mutex;
use driver_abi::{
    DriverBus, DriverClass, DriverInitFn, DriverModuleHeader, RUSTOS_DRIVER_ABI_VERSION_SYMBOL,
    RUSTOS_DRIVER_HEADER_SYMBOL, RUSTOS_DRIVER_INIT_SYMBOL,
};
use elf::ElfBytes;
use elf::endian::LittleEndian as ElfLittleEndian;
use elf::file::Class as ElfClass;
use object::LittleEndian;
use object::elf::{
    self as objelf, FileHeader64 as RawElfHeader, Rela64 as RawRela,
    SectionHeader64 as RawSectionHeader, Sym64 as RawSym,
};
use x86_64::PhysAddr;

use crate::sync::KernelWaitLock;

use super::{bus, class, export, module_registry};

macro_rules! driver_diag {
    ($level:expr, $event_id:expr, $object_id:expr, $($arg:tt)*) => {{
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "driver-loader",
            ($event_id) as u64,
            ($object_id) as u64,
        );
        if crate::debug::should_emit(crate::debug::LogCategory::Driver, $level) {
            crate::debug::log_args_site(
                crate::debug::LogCategory::Driver,
                $level,
                module_path!(),
                line!(),
                format_args!($($arg)*),
            );
        }
    }};
}

const LINUX_COMPAT_INIT_SYMBOL: &str = "init_module";
const MODULE_PAGE_SIZE: usize = 4096;
const MAX_MODULE_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_MODULE_ALLOC_BYTES: usize = 64 * 1024 * 1024;
const MAX_MODULE_SECTIONS: usize = 16384;
const MAX_MODULE_SYMBOLS: usize = 65536;
const MAX_MODULE_RELOCATIONS: usize = 262144;
const MAX_MODULE_SECTION_ALIGN: usize = 64 * 1024;
const MAX_MODULE_ARENA_BYTES: usize = 128 * 1024 * 1024;
const MAX_COMPAT_TRAMPOLINES: usize = 4096;
const TRAMPOLINE_SIZE: usize = 64;
const TRAMPOLINES_PER_PAGE: usize = MODULE_PAGE_SIZE / TRAMPOLINE_SIZE;
const MODULE_INIT_STACK_SIZE: usize = 16 * 1024 * 1024;

const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;
const R_X86_64_PC64: u32 = 24;
const SHT_X86_64_UNWIND: u32 = 0x7000_0001;
const ELF_ENDIAN: LittleEndian = LittleEndian;

#[derive(Clone, Copy)]
pub(super) struct ModuleElf<'a> {
    input: &'a [u8],
    header: RawElfHeader<LittleEndian>,
}

impl<'a> ModuleElf<'a> {
    fn parse(input: &'a [u8]) -> Result<Self, &'static str> {
        let parsed = ElfBytes::<ElfLittleEndian>::minimal_parse(input)
            .map_err(|_| "module ELF is invalid")?;
        if parsed.ehdr.class != ElfClass::ELF64 {
            return Err("module ELF is invalid");
        }
        let header = read_raw_elf_header(input)?;
        Ok(Self { input, header })
    }
}

static LOADED_MODULES: Mutex<Vec<LoadedDriverModule>> = Mutex::new(Vec::new());
static PENDING_MODULE_EXEC_RANGES: Mutex<Vec<PendingModuleExecRange>> = Mutex::new(Vec::new());
static KERNEL_COMPAT_TRAMPOLINES: Mutex<Vec<KernelCompatTrampoline>> = Mutex::new(Vec::new());
static KERNEL_COMPAT_TRAMPOLINE_PAGES: Mutex<Vec<KernelCompatTrampolinePage>> =
    Mutex::new(Vec::new());
static MODULE_INIT_STACK_LOCK: KernelWaitLock<()> = KernelWaitLock::new(());
static MODULE_ARENA_BYTES: AtomicUsize = AtomicUsize::new(0);

fn allocate_module_init_stack() -> Option<Vec<u8>> {
    let mut stack = Vec::new();
    stack.try_reserve_exact(MODULE_INIT_STACK_SIZE).ok()?;
    unsafe {
        stack.set_len(MODULE_INIT_STACK_SIZE);
    }
    Some(stack)
}

#[inline(always)]
unsafe fn call_with_module_init_stack0(entry: usize) -> i32 {
    let _guard = MODULE_INIT_STACK_LOCK.lock();
    let mut stack = allocate_module_init_stack().expect("module init stack allocation failed");
    let stack_base = stack.as_mut_ptr() as usize;
    let stack_top = stack_base + stack.len();
    let _stack_scope =
        kernel_ps::api::CurrentKernelStackScope::enter(stack_base as u64, stack_top as u64)
            .expect("scheduler must accept module init stack bounds");
    let mut result: i32;
    // x86-64 Linux kernel code is built for an 8-byte stack boundary. Enter
    // compat module init with rsp % 16 == 0; after the CALL pushes its return
    // address, this matches the kernel module compiler's expected parity.
    unsafe {
        asm!(
            "push rbx",
            "mov rbx, rsp",
            "mov rsp, rdx",
            "and rsp, -16",
            "sub rsp, 8",
            "call rax",
            "mov rsp, rbx",
            "pop rbx",
            in("rax") entry,
            in("rdx") stack_top,
            lateout("eax") result,
            clobber_abi("C"),
        );
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleMemoryClass {
    CoreText,
    CoreRodata,
    CoreRoAfterInit,
    CoreData,
    InitText,
    InitRodata,
    InitData,
}

impl ModuleMemoryClass {
    const COUNT: usize = 7;
    const ALL: [Self; Self::COUNT] = [
        Self::CoreText,
        Self::CoreRodata,
        Self::CoreRoAfterInit,
        Self::CoreData,
        Self::InitText,
        Self::InitRodata,
        Self::InitData,
    ];
    const EXECUTABLE: [Self; 2] = [Self::CoreText, Self::InitText];

    const fn index(self) -> usize {
        match self {
            Self::CoreText => 0,
            Self::CoreRodata => 1,
            Self::CoreRoAfterInit => 2,
            Self::CoreData => 3,
            Self::InitText => 4,
            Self::InitRodata => 5,
            Self::InitData => 6,
        }
    }

    const fn is_text(self) -> bool {
        matches!(self, Self::CoreText | Self::InitText)
    }

    const fn is_rodata(self) -> bool {
        matches!(self, Self::CoreRodata | Self::InitRodata)
    }

    const fn is_init(self) -> bool {
        matches!(self, Self::InitText | Self::InitRodata | Self::InitData)
    }
}

#[derive(Clone, Copy, Default)]
struct ModuleClassRegion {
    runtime_offset: usize,
    used_size: usize,
    mapped_size: usize,
}

impl ModuleClassRegion {
    const fn empty() -> Self {
        Self {
            runtime_offset: 0,
            used_size: 0,
            mapped_size: 0,
        }
    }

    fn has_contents(self) -> bool {
        self.used_size != 0
    }

    fn has_mapped_span(self) -> bool {
        self.mapped_size != 0
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
pub(super) struct LoadedModuleInfo {
    pub(super) name: &'static str,
    pub(super) image_path: &'static str,
    pub(super) runtime_base: usize,
    pub(super) host_base: usize,
}

struct ValidatedModuleImage<'a> {
    elf: ModuleElf<'a>,
    abi: ModuleAbi,
    header: DriverModuleHeader,
}

#[cfg(test)]
pub(super) fn validate_module_image(
    image_path: &str,
    expected_name: &str,
    expected_class: DriverClass,
    expected_bus: DriverBus,
) -> Result<DriverModuleHeader, &'static str> {
    driver_diag!(
        crate::debug::LogLevel::Debug,
        1,
        0,
        "driver validate begin: name={} class={} bus={} path={}",
        expected_name,
        class::name(expected_class),
        bus::name(expected_bus),
        image_path
    );
    let image = crate::storage::boot_volume::read_file_to_vec(image_path)
        .map_err(|_| "module image not found")?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        2,
        0,
        "driver validate read complete: path={} bytes={}",
        image_path,
        image.len()
    );
    let header = parse_validated_module_image(
        image.as_ref(),
        expected_name,
        expected_class,
        expected_bus,
        image_path,
    )?
    .header;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        9,
        0,
        "driver validate header done: path={} abi_version={} class={} bus={}",
        image_path,
        header.abi_version,
        class::name(header.class),
        bus::name(header.bus)
    );

    if header.abi_version != driver_abi::DRIVER_MODULE_ABI_VERSION {
        return Err("driver module ABI version mismatch");
    }
    if !class::is_supported(header.class) {
        return Err("driver module class is unsupported");
    }
    if !bus::is_supported(header.bus) {
        return Err("driver module bus is unsupported");
    }
    if header.class != expected_class {
        return Err("driver module class mismatch");
    }
    if header.bus != expected_bus {
        return Err("driver module bus mismatch");
    }
    if header
        .name_str()
        .map_err(|_| "driver module name is not UTF-8")?
        != expected_name
    {
        return Err("driver module name mismatch");
    }
    if header
        .module_path_str()
        .map_err(|_| "driver module path is not UTF-8")?
        != image_path
    {
        return Err("driver module path mismatch");
    }

    Ok(header)
}

pub(super) fn load_module_image_explicit(
    name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    image_path: &'static str,
    _linux_driver_names: &'static str,
) -> Result<LoadedModuleInfo, &'static str> {
    driver_diag!(
        crate::debug::LogLevel::Debug,
        10,
        0,
        "driver loader: read begin name={} class={} bus={} path={}",
        name,
        class::name(class),
        bus::name(bus),
        image_path,
    );
    let image = crate::storage::boot_volume::read_file_to_vec(image_path)
        .map_err(|_| "module image not found")?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        11,
        0,
        "driver loader: read done name={} bytes={} path={}",
        name,
        image.len(),
        image_path,
    );
    let ValidatedModuleImage { elf, abi, header } =
        parse_validated_module_image(image.as_ref(), name, class, bus, image_path)?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        12,
        0,
        "driver loader: validate done name={} path={}",
        name,
        image_path,
    );
    let policy = SymbolResolvePolicy::new(name, class, bus, abi);
    let (mut memory, layout) = allocate_module_memory(&elf)?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        13,
        memory.runtime_base() as u64,
        "driver loader: allocate done name={} base={:#x} size={:#x} path={}",
        name,
        memory.runtime_base(),
        memory.size(),
        image_path,
    );
    copy_alloc_sections(&elf, memory.host_base(), &layout.sections)?;
    apply_module_relocations(
        &elf,
        memory.host_base(),
        memory.runtime_base(),
        &layout,
        policy,
    )?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        14,
        memory.runtime_base() as u64,
        "driver loader: relocations done name={} path={}",
        name,
        image_path,
    );
    apply_module_memory_permissions_pre_init(&memory)?;
    debug_probe_module_memory(name, &elf, &memory, &layout.sections, policy);
    let mut exec_guard = PendingModuleExecGuard::new(memory.runtime_base(), memory.size());

    let status = match abi {
        ModuleAbi::RustOs(_) => {
            let init_addr = resolve_named_symbol_addr(
                &elf,
                memory.runtime_base(),
                &layout.sections,
                RUSTOS_DRIVER_INIT_SYMBOL,
                ModuleSymbolType::Func,
            )?;
            driver_diag!(
                crate::debug::LogLevel::Info,
                20,
                memory.runtime_base() as u64,
                "driver module init begin: name={} abi=rustos path={} base={:#x} host={:#x} entry={:#x}",
                name,
                image_path,
                memory.runtime_base(),
                memory.host_base() as usize,
                init_addr
            );
            driver_diag!(
                crate::debug::LogLevel::Info,
                24,
                memory.runtime_base() as u64,
                "driver module init call: name={} abi=rustos path={} entry={:#x}",
                name,
                image_path,
                init_addr
            );
            driver_diag!(
                crate::debug::LogLevel::Debug,
                25,
                memory.runtime_base() as u64,
                "driver module init raw-call: name={} abi=rustos path={} entry={:#x}",
                name,
                image_path,
                init_addr
            );
            crate::debug::record_milestone(
                crate::debug::LogCategory::Driver,
                "module-init-rustos-call",
                image.len() as u64,
                init_addr as u64,
            );
            let init: DriverInitFn = unsafe { core::mem::transmute(init_addr) };
            unsafe { init(super::exported_kernel_api()) }
        }
        ModuleAbi::LinuxCompat(_) => match find_symbol(&elf, LINUX_COMPAT_INIT_SYMBOL)? {
            Some((_, init_entry)) => {
                if init_entry.symbol_type()? != ModuleSymbolType::Func {
                    return Err("driver module init symbol type is invalid");
                }
                let init_addr = resolve_named_symbol_addr(
                    &elf,
                    memory.runtime_base(),
                    &layout.sections,
                    LINUX_COMPAT_INIT_SYMBOL,
                    ModuleSymbolType::Func,
                )?;
                driver_diag!(
                    crate::debug::LogLevel::Info,
                    21,
                    memory.runtime_base() as u64,
                    "driver module init begin: name={} abi=linux path={} base={:#x} host={:#x} entry={:#x}",
                    name,
                    image_path,
                    memory.runtime_base(),
                    memory.host_base() as usize,
                    init_addr
                );
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "module-init-linux-call",
                    image.len() as u64,
                    init_addr as u64,
                );
                unsafe { call_with_module_init_stack0(init_addr) }
            }
            None => {
                driver_diag!(
                    crate::debug::LogLevel::Info,
                    21,
                    memory.runtime_base() as u64,
                    "driver module init skipped: name={} abi=linux path={} reason=no-init-symbol",
                    name,
                    image_path
                );
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "module-init-linux-skipped",
                    image.len() as u64,
                    0,
                );
                0
            }
        },
    };
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "module-init-return",
        image.len() as u64,
        status as u32 as u64,
    );
    driver_diag!(
        crate::debug::LogLevel::Debug,
        28,
        memory.runtime_base() as u64,
        "driver module init raw-return: name={} class={} bus={} path={} status={}",
        name,
        class::name(class),
        bus::name(bus),
        image_path,
        status
    );
    driver_diag!(
        crate::debug::LogLevel::Info,
        29,
        memory.runtime_base() as u64,
        "driver module init return: name={} class={} bus={} path={} status={}",
        name,
        class::name(class),
        bus::name(bus),
        image_path,
        status
    );
    driver_diag!(
        crate::debug::LogLevel::Info,
        22,
        memory.runtime_base() as u64,
        "driver module init status: name={} class={} bus={} path={} status={}",
        name,
        class::name(class),
        bus::name(bus),
        image_path,
        status
    );
    if status != 0 {
        return Err("driver module init returned failure");
    }

    seal_module_memory_after_init(&memory)?;

    let exported = module_registry::register_module_exports(name, &elf, &memory, &layout)?;
    if exported != 0 {
        driver_diag!(
            crate::debug::LogLevel::Info,
            23,
            memory.runtime_base() as u64,
            "driver module exports registered: name={} path={} count={}",
            name,
            image_path,
            exported
        );
    }

    reclaim_module_init_memory(&mut memory)?;

    let info = LoadedModuleInfo {
        name,
        image_path,
        runtime_base: memory.runtime_base(),
        host_base: memory.host_base() as usize,
    };
    LOADED_MODULES.lock().push(LoadedDriverModule {
        _name: name,
        _image_path: image_path,
        _header: header,
        _memory: memory,
    });
    exec_guard.disarm();
    Ok(info)
}

pub(super) fn read_string_table_entry<'a>(
    table: &'a [u8],
    offset: u32,
    range_error: &'static str,
    utf8_error: &'static str,
) -> Result<&'a str, &'static str> {
    let start = usize::try_from(offset).map_err(|_| range_error)?;
    let bytes = table.get(start..).ok_or(range_error)?;
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(range_error)?;
    core::str::from_utf8(&bytes[..end]).map_err(|_| utf8_error)
}

pub(super) fn section_header_string_table<'a>(
    elf: &'a ModuleElf<'a>,
) -> Result<&'a [u8], &'static str> {
    let elf_header = module_elf_header(elf)?;
    let sections = section_header_entries(elf)?;
    let section = sections
        .get(usize::from(elf_header.e_shstrndx.get(ELF_ENDIAN)))
        .copied()
        .ok_or("module ELF shstr section is invalid")?;
    if section.section_type != objelf::SHT_STRTAB {
        return Err("module ELF shstr section format is invalid");
    }

    section_data_bytes_raw(elf, &section, "module ELF shstr section is invalid")
}

pub(super) fn add_signed_usize(value: usize, addend: i64) -> Result<usize, &'static str> {
    if addend >= 0 {
        value
            .checked_add(addend as usize)
            .ok_or("module relocation value overflow")
    } else {
        value
            .checked_sub(addend.unsigned_abs() as usize)
            .ok_or("module relocation value underflow")
    }
}

#[cfg(test)]
pub(super) fn reset_for_tests() {
    LOADED_MODULES.lock().clear();
    PENDING_MODULE_EXEC_RANGES.lock().clear();
    KERNEL_COMPAT_TRAMPOLINES.lock().clear();
    KERNEL_COMPAT_TRAMPOLINE_PAGES.lock().clear();
    MODULE_ARENA_BYTES.store(0, Ordering::Release);
}

pub(crate) fn runtime_executable_addr_is_known(addr: usize) -> bool {
    #[cfg(not(test))]
    {
        let addr_u64 = addr as u64;
        if addr_u64 >= crate::memory::paging::KERNEL_VIRT_OFFSET {
            let phys = crate::memory::paging::kernel_virtual_to_physical_addr(addr_u64);
            if crate::memory::paging::direct_map_phys_is_executable(phys) {
                return true;
            }
        } else if addr_u64 < crate::memory::kernel_vm::DIRECT_MAP_PHYS_LIMIT
            && crate::memory::paging::direct_map_phys_is_executable(addr_u64)
        {
            return true;
        }
    }

    {
        let modules = LOADED_MODULES.lock();
        if modules
            .iter()
            .any(|module| module._memory.contains_executable_addr(addr))
        {
            return true;
        }
    }

    {
        let pending = PENDING_MODULE_EXEC_RANGES.lock();
        if pending.iter().any(|range| {
            let end = range.start.saturating_add(range.size);
            (range.start..end).contains(&addr)
        }) {
            return true;
        }
    }

    KERNEL_COMPAT_TRAMPOLINES
        .lock()
        .iter()
        .any(|entry| entry.runtime_addr == addr)
}

pub(super) struct ModuleMemory {
    allocation: ModuleArenaAllocation,
    size: usize,
    runtime_base: usize,
    class_regions: [ModuleClassRegion; ModuleMemoryClass::COUNT],
}

impl ModuleMemory {
    pub(super) fn host_base(&self) -> *mut u8 {
        self.allocation.host_base()
    }

    pub(super) fn runtime_base(&self) -> usize {
        self.runtime_base
    }

    pub(super) fn size(&self) -> usize {
        self.size
    }

    fn mapped_phys_base(&self) -> u64 {
        self.allocation.mapped_phys_base()
    }

    fn phys_region_for_class(&self, class: ModuleMemoryClass) -> Option<(u64, usize)> {
        let region = self.class_regions[class.index()];
        if !region.has_mapped_span() {
            return None;
        }
        let phys = self
            .mapped_phys_base()
            .checked_add(region.runtime_offset as u64)?;
        Some((phys, region.mapped_size))
    }

    fn runtime_region_for_class(&self, class: ModuleMemoryClass) -> Option<(usize, usize)> {
        let region = self.class_regions[class.index()];
        if !region.has_contents() {
            return None;
        }
        let start = self.runtime_base.checked_add(region.runtime_offset)?;
        Some((start, region.used_size))
    }

    fn contains_executable_addr(&self, addr: usize) -> bool {
        ModuleMemoryClass::EXECUTABLE.iter().copied().any(|class| {
            self.runtime_region_for_class(class)
                .is_some_and(|(start, size)| {
                    let end = start.saturating_add(size);
                    (start..end).contains(&addr)
                })
        })
    }
}

impl Drop for ModuleMemory {
    fn drop(&mut self) {
        let _ = self.size;
    }
}

unsafe impl Send for ModuleMemory {}

#[derive(Clone, Copy)]
struct PendingModuleExecRange {
    start: usize,
    size: usize,
}

struct PendingModuleExecGuard {
    start: usize,
    size: usize,
    active: bool,
}

impl PendingModuleExecGuard {
    fn new(start: usize, size: usize) -> Self {
        PENDING_MODULE_EXEC_RANGES
            .lock()
            .push(PendingModuleExecRange { start, size });
        Self {
            start,
            size,
            active: true,
        }
    }

    fn disarm(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        remove_pending_module_exec_range(self.start, self.size);
    }
}

impl Drop for PendingModuleExecGuard {
    fn drop(&mut self) {
        if self.active {
            remove_pending_module_exec_range(self.start, self.size);
        }
    }
}

fn remove_pending_module_exec_range(start: usize, size: usize) {
    let mut pending = PENDING_MODULE_EXEC_RANGES.lock();
    if let Some(index) = pending
        .iter()
        .position(|range| range.start == start && range.size == size)
    {
        pending.swap_remove(index);
    }
}

struct LoadedDriverModule {
    _name: &'static str,
    _image_path: &'static str,
    _header: DriverModuleHeader,
    _memory: ModuleMemory,
}

unsafe impl Send for LoadedDriverModule {}

struct KernelCompatTrampoline {
    target_addr: usize,
    mode: CompatTrampolineMode,
    runtime_addr: usize,
}

unsafe impl Send for KernelCompatTrampoline {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompatTrampolineMode {
    AlignRustCall,
    PreserveStackTail,
}

struct ModuleArenaAllocation {
    phys_start: PhysAddr,
    page_count: usize,
    raw_host_ptr: NonNull<u8>,
    host_ptr: NonNull<u8>,
    host_offset: usize,
    raw_len: usize,
}

impl ModuleArenaAllocation {
    fn host_base(&self) -> *mut u8 {
        self.host_ptr.as_ptr()
    }

    fn mapped_phys_base(&self) -> u64 {
        self.phys_start.as_u64() + self.host_offset as u64
    }
}

impl Drop for ModuleArenaAllocation {
    fn drop(&mut self) {
        unsafe {
            ptr::write_bytes(self.raw_host_ptr.as_ptr(), 0, self.raw_len);
        }
        for page_index in 0..self.page_count {
            let phys = self
                .phys_start
                .as_u64()
                .checked_add((page_index * MODULE_PAGE_SIZE) as u64)
                .expect("module arena free address overflow");
            crate::memory::phys::free_frame(PhysAddr::new(phys));
        }
        MODULE_ARENA_BYTES.fetch_sub(self.raw_len, Ordering::AcqRel);
    }
}

unsafe impl Send for ModuleArenaAllocation {}

struct KernelCompatTrampolinePage {
    allocation: ModuleArenaAllocation,
    used: [bool; TRAMPOLINES_PER_PAGE],
}

impl KernelCompatTrampolinePage {
    fn new() -> Result<Self, &'static str> {
        let allocation = allocate_module_arena(MODULE_PAGE_SIZE, MODULE_PAGE_SIZE)?;
        Ok(Self {
            allocation,
            used: [false; TRAMPOLINES_PER_PAGE],
        })
    }

    fn has_free_slot(&self) -> bool {
        self.used.iter().any(|used| !*used)
    }

    fn prepare_for_write(&mut self) -> Result<(), &'static str> {
        if crate::memory::paging::mark_direct_map_range_writable_noexec(
            self.allocation.phys_start.as_u64(),
            MODULE_PAGE_SIZE,
        ) {
            Ok(())
        } else {
            Err("failed to mark compat trampoline page writable")
        }
    }

    fn seal_executable(&mut self) -> Result<(), &'static str> {
        if crate::memory::paging::mark_direct_map_range_executable(
            self.allocation.phys_start.as_u64(),
            MODULE_PAGE_SIZE,
        ) {
            Ok(())
        } else {
            Err("failed to mark compat trampoline page executable")
        }
    }

    fn allocate_slot(&mut self) -> Option<(*mut u8, usize)> {
        let slot_index = self.used.iter().position(|used| !*used)?;
        self.used[slot_index] = true;
        let host_ptr = unsafe {
            self.allocation
                .host_base()
                .add(slot_index * TRAMPOLINE_SIZE)
        };
        let runtime_addr = crate::memory::paging::lower_half_addr(host_ptr as u64) as usize;
        Some((host_ptr, runtime_addr))
    }
}

unsafe impl Send for KernelCompatTrampolinePage {}

fn apply_module_memory_permissions_pre_init(memory: &ModuleMemory) -> Result<(), &'static str> {
    for class in ModuleMemoryClass::ALL {
        let Some((phys_addr, size)) = memory.phys_region_for_class(class) else {
            continue;
        };
        let ok = if class.is_text() {
            crate::memory::paging::mark_direct_map_range_executable(phys_addr, size)
        } else if class.is_rodata() {
            crate::memory::paging::mark_direct_map_range_readonly_noexec(phys_addr, size)
        } else {
            crate::memory::paging::mark_direct_map_range_writable_noexec(phys_addr, size)
        };
        if !ok {
            return Err("failed to apply module memory permissions");
        }
    }
    Ok(())
}

fn seal_module_memory_after_init(memory: &ModuleMemory) -> Result<(), &'static str> {
    let Some((phys_addr, size)) = memory.phys_region_for_class(ModuleMemoryClass::CoreRoAfterInit)
    else {
        return Ok(());
    };
    if crate::memory::paging::mark_direct_map_range_readonly_noexec(phys_addr, size) {
        Ok(())
    } else {
        Err("failed to seal module ro_after_init memory")
    }
}

fn reclaim_module_init_memory(memory: &mut ModuleMemory) -> Result<(), &'static str> {
    for class in ModuleMemoryClass::ALL {
        if !class.is_init() {
            continue;
        }

        let region = memory.class_regions[class.index()];
        if !region.has_contents() {
            continue;
        }

        let Some((phys_addr, mapped_size)) = memory.phys_region_for_class(class) else {
            continue;
        };
        if !crate::memory::paging::mark_direct_map_range_writable_noexec(phys_addr, mapped_size) {
            return Err("failed to prepare module init memory for reclaim");
        }

        unsafe {
            ptr::write_bytes(
                memory.host_base().add(region.runtime_offset),
                0,
                region.used_size,
            );
        }

        if !crate::memory::paging::mark_direct_map_range_readonly_noexec(phys_addr, mapped_size) {
            return Err("failed to seal reclaimed module init memory");
        }

        memory.class_regions[class.index()] = ModuleClassRegion::empty();
    }

    Ok(())
}

fn debug_probe_module_memory(
    name: &str,
    elf: &ModuleElf<'_>,
    memory: &ModuleMemory,
    layouts: &[Option<ModuleSectionLayout>],
    policy: SymbolResolvePolicy,
) {
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "module-probe-entry",
        stable_ascii_hash(name),
        memory.size() as u64,
    );
    if !matches!(
        name,
        "hid" | "usbhid" | "psmouse" | "virtio_net" | "virtio-net"
    ) {
        return;
    }

    for class in ModuleMemoryClass::ALL {
        let region = memory.class_regions[class.index()];
        if !region.has_contents() {
            continue;
        }
        let Some((phys, mapped_size)) = memory.phys_region_for_class(class) else {
            continue;
        };
        let flags = crate::memory::kernel_vm::debug_direct_map_flags_for_addr(phys)
            .map(|entry| entry.bits())
            .unwrap_or(0);
        driver_diag!(
            crate::debug::LogLevel::Debug,
            30,
            memory.runtime_base() as u64,
            "driver module probe: name={} class={:?} runtime_off={:#x} used={:#x} mapped={:#x} phys={:#x} flags={:#x}",
            name,
            class,
            region.runtime_offset,
            region.used_size,
            mapped_size,
            phys,
            flags,
        );
    }

    match name {
        "hid" => {
            debug_probe_writable_symbol_u32(name, "hidraw_major", elf, memory, layouts);
            debug_probe_relocation_target(
                name,
                ".rela.text",
                "sscanf",
                elf,
                memory,
                layouts,
                policy,
            );
        }
        "usbhid" => {
            debug_probe_writable_symbol_u32(name, "quirks_param", elf, memory, layouts);
            debug_probe_writable_symbol_u32(name, "hid_mousepoll_interval", elf, memory, layouts);
            debug_probe_relocation_target(
                name,
                ".rela.init.text",
                "hid_quirks_init",
                elf,
                memory,
                layouts,
                policy,
            );
            debug_probe_relocation_target(
                name,
                ".rela.init.text",
                "usb_register_driver",
                elf,
                memory,
                layouts,
                policy,
            );
        }
        "psmouse" => {
            debug_probe_relocation_target(
                name,
                ".rela.init.text",
                "alloc_workqueue",
                elf,
                memory,
                layouts,
                policy,
            );
            debug_probe_relocation_target(
                name,
                ".rela.init.text",
                "bus_register_notifier",
                elf,
                memory,
                layouts,
                policy,
            );
            debug_probe_relocation_target(
                name,
                ".rela.init.text",
                "__x86_return_thunk",
                elf,
                memory,
                layouts,
                policy,
            );
        }
        "virtio_net" | "virtio-net" => {
            crate::debug::record_milestone(
                crate::debug::LogCategory::Driver,
                "module-probe-virtio-net",
                memory.runtime_base() as u64,
                memory.size() as u64,
            );
            debug_probe_relocation_target(
                name,
                ".rela.init.text",
                "__register_virtio_driver",
                elf,
                memory,
                layouts,
                policy,
            );
            debug_probe_relocation_target(
                name,
                ".rela.init.text",
                "__x86_return_thunk",
                elf,
                memory,
                layouts,
                policy,
            );
        }
        _ => {}
    }
}

fn stable_ascii_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn debug_probe_writable_symbol_u32(
    module_name: &str,
    symbol_name: &str,
    elf: &ModuleElf<'_>,
    memory: &ModuleMemory,
    layouts: &[Option<ModuleSectionLayout>],
) {
    let Ok(Some((_, symbol))) = find_symbol(elf, symbol_name) else {
        return;
    };
    let Ok(runtime_addr) = resolve_symbol_addr(
        elf,
        memory.runtime_base(),
        layouts,
        &symbol,
        R_X86_64_64,
        SymbolResolvePolicy::kernel_internal(),
    ) else {
        return;
    };
    let Some(layout) = layouts
        .get(usize::from(symbol.shndx))
        .and_then(|entry| *entry)
    else {
        return;
    };
    let Ok(symbol_offset) = usize::try_from(symbol.value) else {
        return;
    };
    if symbol_offset
        .checked_add(size_of::<u32>())
        .is_none_or(|end| end > layout.size)
    {
        return;
    }
    let host_ptr = unsafe {
        memory
            .host_base()
            .add(layout.runtime_offset)
            .add(symbol_offset) as *mut u32
    };
    let flags = crate::memory::kernel_vm::debug_direct_map_flags_for_addr(runtime_addr as u64)
        .map(|entry| entry.bits())
        .unwrap_or(0);
    let before = unsafe { ptr::read_volatile(host_ptr) };
    unsafe {
        ptr::write_volatile(host_ptr, before);
    }
    let after = unsafe { ptr::read_volatile(host_ptr) };
    driver_diag!(
        crate::debug::LogLevel::Debug,
        31,
        runtime_addr as u64,
        "driver module probe: name={} symbol={} runtime={:#x} host={:#x} before={:#x} after={:#x} flags={:#x}",
        module_name,
        symbol_name,
        runtime_addr,
        host_ptr as usize,
        before,
        after,
        flags,
    );
}

fn debug_probe_relocation_target(
    module_name: &str,
    relocation_section_name: &str,
    symbol_name: &str,
    elf: &ModuleElf<'_>,
    memory: &ModuleMemory,
    layouts: &[Option<ModuleSectionLayout>],
    policy: SymbolResolvePolicy,
) {
    let Ok(section_headers) = section_header_entries(elf) else {
        return;
    };
    let Ok(section_names) = section_header_string_table(elf) else {
        return;
    };
    let Ok(symbol_table) = symbol_table_entries(elf) else {
        return;
    };
    let Ok(symbol_names) = symbol_string_table(elf) else {
        return;
    };

    for reloc_header in section_headers.iter().copied() {
        if reloc_header.section_type != objelf::SHT_RELA {
            continue;
        }
        let Ok(reloc_name) = read_string_table_entry(
            section_names,
            reloc_header.name,
            "module section name offset is out of range",
            "module section name is not UTF-8",
        ) else {
            continue;
        };
        if reloc_name != relocation_section_name {
            continue;
        }

        let Ok(target_index) = usize::try_from(reloc_header.info) else {
            continue;
        };
        let Some(target_layout) = layouts.get(target_index).and_then(|entry| *entry) else {
            continue;
        };
        let Ok(relocations) = relocation_entries_by_header(elf, reloc_header) else {
            continue;
        };
        for relocation in relocations {
            if !matches!(relocation.relocation_type(), R_X86_64_PC32 | R_X86_64_PLT32) {
                continue;
            }
            let symbol_index = relocation.symbol_index() as usize;
            let Some(symbol) = symbol_table.get(symbol_index) else {
                continue;
            };
            let Ok(candidate_name) = symbol_name_from_table(symbol_names, symbol) else {
                continue;
            };
            if candidate_name != symbol_name {
                continue;
            }

            let Ok(reloc_offset) = usize::try_from(relocation.offset()) else {
                continue;
            };
            if reloc_offset
                .checked_add(size_of::<i32>())
                .is_none_or(|end| end > target_layout.size)
            {
                continue;
            }
            let write_runtime_addr = match memory
                .runtime_base()
                .checked_add(target_layout.runtime_offset)
                .and_then(|addr| addr.checked_add(reloc_offset))
            {
                Some(addr) => addr,
                None => continue,
            };
            let write_host_ptr = unsafe {
                memory
                    .host_base()
                    .add(target_layout.runtime_offset)
                    .add(reloc_offset) as *const i32
            };
            let displacement = unsafe { ptr::read_unaligned(write_host_ptr) };
            let patched_target = match add_signed_usize(
                write_runtime_addr + size_of::<i32>(),
                displacement as i64,
            ) {
                Ok(addr) => addr,
                Err(_) => continue,
            };
            let Ok(resolved_target) = resolve_symbol_addr(
                elf,
                memory.runtime_base(),
                layouts,
                symbol,
                relocation.relocation_type(),
                policy,
            ) else {
                continue;
            };
            let trampoline_target = compat_trampoline_target(patched_target).unwrap_or(0);
            let target_head = unsafe {
                ptr::read_unaligned(
                    crate::memory::paging::higher_half_addr(patched_target as u64) as *const u64,
                )
            };
            if matches!(module_name, "virtio_net" | "virtio-net")
                && matches!(
                    symbol_name,
                    "__register_virtio_driver" | "__x86_return_thunk"
                )
            {
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "module-reloc-target",
                    write_runtime_addr as u64,
                    patched_target as u64,
                );
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "module-reloc-resolved",
                    resolved_target as u64,
                    trampoline_target as u64,
                );
                crate::debug::record_milestone(
                    crate::debug::LogCategory::Driver,
                    "module-reloc-head",
                    target_head,
                    relocation.relocation_type() as u64,
                );
            }
            driver_diag!(
                crate::debug::LogLevel::Debug,
                32,
                write_runtime_addr as u64,
                "driver module probe: name={} reloc_section={} symbol={} write={:#x} disp={:#x} patched_target={:#x} resolved_target={:#x} trampoline_target={:#x} target_head={:#x}",
                module_name,
                relocation_section_name,
                symbol_name,
                write_runtime_addr,
                displacement,
                patched_target,
                resolved_target,
                trampoline_target,
                target_head,
            );
            if symbol_name != "__x86_return_thunk" {
                return;
            }
        }
    }
}

fn compat_trampoline_target(runtime_addr: usize) -> Option<usize> {
    let trampolines = KERNEL_COMPAT_TRAMPOLINES.lock();
    trampolines
        .iter()
        .find(|entry| entry.runtime_addr == runtime_addr)
        .map(|entry| entry.target_addr)
}

#[derive(Clone, Copy)]
enum ModuleAbi {
    RustOs(DriverModuleHeader),
    LinuxCompat(DriverModuleHeader),
}

impl ModuleAbi {
    fn header(self) -> DriverModuleHeader {
        match self {
            Self::RustOs(header) | Self::LinuxCompat(header) => header,
        }
    }
}

fn validate_module_path(image_path: &str) -> Result<(), &'static str> {
    if image_path.is_empty()
        || !image_path.starts_with("system/drivers/")
        || !image_path.ends_with(".ko")
        || image_path.starts_with('/')
        || image_path.contains("//")
        || image_path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err("driver module path is invalid");
    }
    Ok(())
}

fn parse_validated_module_image<'a>(
    input: &'a [u8],
    expected_name: &str,
    expected_class: DriverClass,
    expected_bus: DriverBus,
    image_path: &str,
) -> Result<ValidatedModuleImage<'a>, &'static str> {
    validate_module_path(image_path)?;
    if input.len() > MAX_MODULE_IMAGE_BYTES {
        return Err("module image exceeds hard size cap");
    }

    driver_diag!(
        crate::debug::LogLevel::Debug,
        3,
        0,
        "driver validate parse begin: path={}",
        image_path
    );
    let elf = ModuleElf::parse(input)?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        4,
        0,
        "driver validate parse done: path={}",
        image_path
    );
    driver_diag!(
        crate::debug::LogLevel::Debug,
        5,
        0,
        "driver validate elf header begin: path={}",
        image_path
    );
    let elf_header = module_elf_header(&elf)?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        6,
        0,
        "driver validate elf header done: path={} type={:#x} machine={:#x} shnum={} shentsize={} shoff={:#x}",
        image_path,
        elf_header.e_type.get(ELF_ENDIAN),
        elf_header.e_machine.get(ELF_ENDIAN),
        elf_header.e_shnum.get(ELF_ENDIAN),
        elf_header.e_shentsize.get(ELF_ENDIAN),
        elf_header.e_shoff.get(ELF_ENDIAN)
    );
    if elf_header.e_type.get(ELF_ENDIAN) != objelf::ET_REL {
        return Err("module ELF is not relocatable");
    }
    if elf_header.e_machine.get(ELF_ENDIAN) != objelf::EM_X86_64 {
        return Err("module ELF machine is not x86_64");
    }

    driver_diag!(
        crate::debug::LogLevel::Debug,
        7,
        0,
        "driver validate abi detect begin: path={}",
        image_path
    );
    let abi = detect_module_abi(
        &elf,
        expected_name,
        expected_class,
        expected_bus,
        image_path,
    )?;
    driver_diag!(
        crate::debug::LogLevel::Debug,
        8,
        0,
        "driver validate abi detect done: path={}",
        image_path
    );

    Ok(ValidatedModuleImage {
        elf,
        abi,
        header: abi.header(),
    })
}

#[derive(Clone, Copy)]
struct SymbolResolvePolicy {
    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    module_name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    abi: ModuleAbi,
}

impl SymbolResolvePolicy {
    fn new(module_name: &'static str, class: DriverClass, bus: DriverBus, abi: ModuleAbi) -> Self {
        Self {
            module_name,
            class,
            bus,
            abi,
        }
    }

    fn kernel_internal() -> Self {
        Self {
            module_name: "<internal>",
            class: DriverClass::Input,
            bus: DriverBus::Platform,
            abi: ModuleAbi::RustOs(DriverModuleHeader::from_runtime(
                DriverClass::Input,
                DriverBus::Platform,
                "",
                "",
            )),
        }
    }

    fn is_linux_compat(self) -> bool {
        matches!(self.abi, ModuleAbi::LinuxCompat(_))
    }
}

#[derive(Clone, Copy)]
pub(super) struct ModuleSectionLayout {
    pub(super) runtime_offset: usize,
    pub(super) size: usize,
}

pub(super) struct ModuleLoadLayout {
    pub(super) sections: Vec<Option<ModuleSectionLayout>>,
    got_entries: Vec<Option<usize>>,
}

#[derive(Clone, Copy)]
struct ModuleSymbol {
    name: u32,
    info: u8,
    shndx: u16,
    value: u64,
    size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleSymbolBinding {
    Local,
    Global,
    Weak,
    OsSpecific(u8),
    ProcessorSpecific(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleSymbolType {
    NoType,
    Object,
    Func,
    Section,
    File,
    Common,
    Tls,
    OsSpecific(u8),
    ProcessorSpecific(u8),
}

impl ModuleSymbol {
    fn binding(self) -> Result<ModuleSymbolBinding, &'static str> {
        match self.info >> 4 {
            0 => Ok(ModuleSymbolBinding::Local),
            1 => Ok(ModuleSymbolBinding::Global),
            2 => Ok(ModuleSymbolBinding::Weak),
            value @ 10..=12 => Ok(ModuleSymbolBinding::OsSpecific(value)),
            value @ 13..=15 => Ok(ModuleSymbolBinding::ProcessorSpecific(value)),
            _ => Err("module symbol binding is invalid"),
        }
    }

    fn symbol_type(self) -> Result<ModuleSymbolType, &'static str> {
        match self.info & 0x0f {
            0 => Ok(ModuleSymbolType::NoType),
            1 => Ok(ModuleSymbolType::Object),
            2 => Ok(ModuleSymbolType::Func),
            3 => Ok(ModuleSymbolType::Section),
            4 => Ok(ModuleSymbolType::File),
            5 => Ok(ModuleSymbolType::Common),
            6 => Ok(ModuleSymbolType::Tls),
            value @ 10..=12 => Ok(ModuleSymbolType::OsSpecific(value)),
            value @ 13..=15 => Ok(ModuleSymbolType::ProcessorSpecific(value)),
            _ => Err("module symbol type is invalid"),
        }
    }
}

#[derive(Clone, Copy)]
struct ModuleRela {
    offset: u64,
    info: u64,
    addend: i64,
}

impl ModuleRela {
    fn offset(self) -> u64 {
        self.offset
    }

    fn symbol_index(self) -> u32 {
        (self.info >> 32) as u32
    }

    fn relocation_type(self) -> u32 {
        self.info as u32
    }

    fn addend(self) -> i64 {
        self.addend
    }
}

#[derive(Clone, Copy)]
pub(super) struct ModuleSectionHeader {
    pub(super) name: u32,
    pub(super) section_type: u32,
    pub(super) flags: u64,
    pub(super) offset: u64,
    pub(super) size: u64,
    pub(super) link: u32,
    pub(super) info: u32,
    pub(super) align: u64,
    pub(super) entry_size: u64,
}

#[derive(Clone, Copy)]
enum SymbolResolveFlavor {
    Direct,
    LowAlias,
    Trampoline,
}

impl SymbolResolveFlavor {
    const COUNT: usize = 3;

    const fn index(self) -> usize {
        match self {
            Self::Direct => 0,
            Self::LowAlias => 1,
            Self::Trampoline => 2,
        }
    }
}

type SymbolResolveCacheEntry = [Option<Result<usize, &'static str>>; SymbolResolveFlavor::COUNT];

fn is_supported_alloc_section_type(section_type: u32) -> bool {
    matches!(
        section_type,
        objelf::SHT_PROGBITS | objelf::SHT_NOBITS | objelf::SHT_NOTE | SHT_X86_64_UNWIND
    )
}

fn is_exit_layout_section(section_name: &str) -> bool {
    section_name.starts_with(".exit")
}

fn is_init_layout_section(section_name: &str) -> bool {
    section_name.starts_with(".init") || is_exit_layout_section(section_name)
}

fn is_ro_after_init_section(section_name: &str) -> bool {
    matches!(section_name, "__jump_table" | ".static_call_sites")
        || section_name.starts_with(".data..ro_after_init")
}

fn is_relro_section(section_name: &str) -> bool {
    section_name.contains(".data.rel.ro") || section_name.contains(".bss.rel.ro")
}

fn classify_module_section(
    section: ModuleSectionHeader,
    section_name: &str,
) -> Result<ModuleMemoryClass, &'static str> {
    let is_write = section.flags & u64::from(objelf::SHF_WRITE) != 0;
    let is_exec = section.flags & u64::from(objelf::SHF_EXECINSTR) != 0;
    if is_write && is_exec {
        return Err("module alloc section has invalid WRITE|EXEC flags");
    }

    let is_init = is_init_layout_section(section_name);
    if is_exec {
        return Ok(if is_init {
            ModuleMemoryClass::InitText
        } else {
            ModuleMemoryClass::CoreText
        });
    }

    if is_relro_section(section_name) || !is_write {
        return Ok(if is_init {
            ModuleMemoryClass::InitRodata
        } else {
            ModuleMemoryClass::CoreRodata
        });
    }

    if is_ro_after_init_section(section_name) && !is_init {
        return Ok(ModuleMemoryClass::CoreRoAfterInit);
    }

    Ok(if is_init {
        ModuleMemoryClass::InitData
    } else {
        ModuleMemoryClass::CoreData
    })
}

fn detect_module_abi(
    elf: &ModuleElf<'_>,
    expected_name: &str,
    expected_class: DriverClass,
    expected_bus: DriverBus,
    image_path: &str,
) -> Result<ModuleAbi, &'static str> {
    ensure_unique_critical_symbols(elf)?;
    let rustos_header_symbol = find_symbol(elf, RUSTOS_DRIVER_HEADER_SYMBOL)?;
    if rustos_header_symbol.is_some() {
        validate_module_exports(elf, rustos_header_symbol)?;
        let header = extract_module_header(elf)?;
        return Ok(ModuleAbi::RustOs(header));
    }

    if let Some((_, init_entry)) = find_symbol(elf, LINUX_COMPAT_INIT_SYMBOL)? {
        if init_entry.symbol_type()? != ModuleSymbolType::Func {
            return Err("driver module init symbol type is invalid");
        }
    }

    Ok(ModuleAbi::LinuxCompat(DriverModuleHeader::from_runtime(
        expected_class,
        expected_bus,
        image_path,
        expected_name,
    )))
}

fn allocate_module_memory(
    elf: &ModuleElf<'_>,
) -> Result<(ModuleMemory, ModuleLoadLayout), &'static str> {
    const R_X86_64_GOTPCREL: u32 = 9;
    const GOT_CLASS: ModuleMemoryClass = ModuleMemoryClass::CoreRodata;

    let section_headers = section_header_entries(elf)?;
    let section_names = section_header_string_table(elf)?;
    let mut sections = vec![None; section_headers.len()];
    let mut section_classes = vec![None; section_headers.len()];
    let mut class_sizes = [0usize; ModuleMemoryClass::COUNT];
    let mut class_alignments = [1usize; ModuleMemoryClass::COUNT];
    let mut max_align = 1usize;

    for (index, section) in section_headers.iter().copied().enumerate() {
        if section.flags & u64::from(objelf::SHF_ALLOC) == 0 {
            continue;
        }
        if !is_supported_alloc_section_type(section.section_type) {
            return Err("module alloc section type is unsupported");
        }
        let size = usize::try_from(section.size).map_err(|_| "module section too large")?;
        if size == 0 {
            continue;
        }
        let section_name = read_string_table_entry(
            section_names,
            section.name,
            "module section name offset is out of range",
            "module section name is not UTF-8",
        )?;
        let class = classify_module_section(section, section_name)?;
        let align = usize::try_from(section.align)
            .map_err(|_| "module section alignment overflow")?
            .max(1);
        if align > MAX_MODULE_SECTION_ALIGN {
            return Err("module section alignment exceeds hard cap");
        }
        let class_index = class.index();
        class_sizes[class_index] =
            align_up(class_sizes[class_index], align).ok_or("module section layout overflow")?;
        section_classes[index] = Some(class);
        class_sizes[class_index] = class_sizes[class_index]
            .checked_add(size)
            .ok_or("module image size overflow")?;
        class_alignments[class_index] = class_alignments[class_index].max(align);
        max_align = max_align.max(align);
    }

    let symbols = symbol_table_entries(elf)?;
    let mut got_entries = vec![None; symbols.len()];
    for reloc_section in section_headers.iter().copied() {
        if reloc_section.section_type != objelf::SHT_RELA {
            continue;
        }
        if !relocation_target_is_loaded(&section_headers, reloc_section)? {
            continue;
        }

        let relocations = relocation_entries_by_header(elf, reloc_section)?;
        for relocation in relocations.iter().copied() {
            if relocation.relocation_type() != R_X86_64_GOTPCREL {
                continue;
            }
            let symbol_index = relocation.symbol_index() as usize;
            let Some(slot) = got_entries.get_mut(symbol_index) else {
                return Err("module relocation symbol is out of range");
            };
            if slot.is_some() {
                continue;
            }
            let got_index = GOT_CLASS.index();
            class_sizes[got_index] = align_up(class_sizes[got_index], size_of::<u64>())
                .ok_or("module GOT layout overflow")?;
            *slot = Some(class_sizes[got_index]);
            class_sizes[got_index] = class_sizes[got_index]
                .checked_add(size_of::<u64>())
                .ok_or("module GOT size overflow")?;
            max_align = max_align.max(size_of::<u64>());
            class_alignments[got_index] = class_alignments[got_index].max(size_of::<u64>());
        }
    }

    let mut total_size = 0usize;
    let mut class_regions = [ModuleClassRegion::empty(); ModuleMemoryClass::COUNT];
    for class in ModuleMemoryClass::ALL {
        let class_index = class.index();
        let used_size = class_sizes[class_index];
        if used_size == 0 {
            continue;
        }
        let class_align = class_alignments[class_index].max(MODULE_PAGE_SIZE);
        total_size = align_up(total_size, class_align).ok_or("module section layout overflow")?;
        class_regions[class_index] = ModuleClassRegion {
            runtime_offset: total_size,
            used_size,
            mapped_size: 0,
        };
        total_size = total_size
            .checked_add(used_size)
            .ok_or("module image size overflow")?;
    }

    let mut next_region_start = total_size;
    for class in ModuleMemoryClass::ALL.iter().rev().copied() {
        let class_index = class.index();
        let region = &mut class_regions[class_index];
        if !region.has_contents() {
            continue;
        }
        region.mapped_size = next_region_start
            .checked_sub(region.runtime_offset)
            .ok_or("module section layout overflow")?;
        next_region_start = region.runtime_offset;
    }

    if total_size == 0 {
        return Err("module ELF contains no alloc sections");
    }
    if total_size > MAX_MODULE_ALLOC_BYTES {
        return Err("module loaded image exceeds hard size cap");
    }

    let mut class_offsets = [0usize; ModuleMemoryClass::COUNT];
    for (index, section) in section_headers.iter().copied().enumerate() {
        let Some(class) = section_classes[index] else {
            continue;
        };
        let size = usize::try_from(section.size).map_err(|_| "module section too large")?;
        let align = usize::try_from(section.align)
            .map_err(|_| "module section alignment overflow")?
            .max(1);
        let class_index = class.index();
        let local_offset =
            align_up(class_offsets[class_index], align).ok_or("module section layout overflow")?;
        let region = class_regions[class_index];
        let runtime_offset = region
            .runtime_offset
            .checked_add(local_offset)
            .ok_or("module section layout overflow")?;
        sections[index] = Some(ModuleSectionLayout {
            runtime_offset,
            size,
        });
        class_offsets[class_index] = local_offset
            .checked_add(size)
            .ok_or("module image size overflow")?;
    }

    let got_base = class_regions[GOT_CLASS.index()].runtime_offset;
    for entry in &mut got_entries {
        if let Some(local_offset) = *entry {
            *entry = Some(
                got_base
                    .checked_add(local_offset)
                    .ok_or("module GOT address overflow")?,
            );
        }
    }

    let allocation = allocate_module_arena(total_size, max_align)?;
    let runtime_base =
        crate::memory::paging::lower_half_addr(allocation.host_base() as u64) as usize;

    Ok((
        ModuleMemory {
            allocation,
            size: total_size,
            runtime_base,
            class_regions,
        },
        ModuleLoadLayout {
            sections,
            got_entries,
        },
    ))
}

fn copy_alloc_sections(
    elf: &ModuleElf<'_>,
    base: *mut u8,
    layouts: &[Option<ModuleSectionLayout>],
) -> Result<(), &'static str> {
    let section_headers = section_header_entries(elf)?;
    for (index, section) in section_headers.iter().copied().enumerate() {
        let Some(layout) = layouts[index] else {
            continue;
        };

        let dest = unsafe { base.add(layout.runtime_offset) };
        if section.section_type == objelf::SHT_NOBITS {
            unsafe {
                ptr::write_bytes(dest, 0, layout.size);
            }
            continue;
        }

        let source = section_data_bytes_raw(elf, &section, "module section data is invalid")?;
        if source.len() < layout.size {
            driver_diag!(
                crate::debug::LogLevel::Warn,
                24,
                index as u64,
                "driver module section truncated: index={} type={:#x} offset={:#x} source_len={:#x} expected={:#x}",
                index,
                section.section_type,
                section.offset,
                source.len(),
                layout.size
            );
            return Err("module section data is truncated");
        }
        unsafe {
            ptr::copy_nonoverlapping(source.as_ptr(), dest, layout.size);
        }
    }

    Ok(())
}

fn apply_module_relocations(
    elf: &ModuleElf<'_>,
    host_base: *mut u8,
    runtime_base: usize,
    layout: &ModuleLoadLayout,
    policy: SymbolResolvePolicy,
) -> Result<(), &'static str> {
    let symbols = symbol_table_entries(elf)?;
    let symbol_names = symbol_string_table(elf)?;
    let section_headers = section_header_entries(elf)?;
    let section_names = section_header_string_table(elf)?;
    let mut resolved_symbols = vec![[None; SymbolResolveFlavor::COUNT]; symbols.len()];

    for reloc_header in section_headers.iter().copied() {
        if reloc_header.section_type != objelf::SHT_RELA {
            continue;
        }
        let reloc_name = read_string_table_entry(
            section_names,
            reloc_header.name,
            "module relocation section name offset is out of range",
            "module relocation section name is not UTF-8",
        )?;
        if should_skip_relocation_section(reloc_name) {
            continue;
        }

        let target_index =
            usize::try_from(reloc_header.info).map_err(|_| "module relocation target overflow")?;
        let Some(target_layout) = layout.sections.get(target_index).and_then(|entry| *entry) else {
            if !relocation_target_index_is_loaded(&section_headers, target_index)? {
                continue;
            }
            return Err("module relocation target section is not loaded");
        };

        let relocations = relocation_entries_by_header(elf, reloc_header)?;
        for relocation in relocations.iter().copied() {
            let reloc_offset = usize::try_from(relocation.offset())
                .map_err(|_| "module relocation offset overflow")?;
            let write_size = relocation_write_size(relocation.relocation_type())?;
            let reloc_end = reloc_offset
                .checked_add(write_size)
                .ok_or("module relocation target overflow")?;
            if reloc_end > target_layout.size {
                return Err("module relocation target is outside section");
            }
            let write_ptr = unsafe { host_base.add(target_layout.runtime_offset) };
            let write_ptr = unsafe { write_ptr.add(reloc_offset) };
            let write_runtime_addr = runtime_base
                .checked_add(target_layout.runtime_offset)
                .and_then(|addr| addr.checked_add(reloc_offset))
                .ok_or("module relocation target overflow")?;
            let addend = relocation.addend();
            let symbol_index = relocation.symbol_index() as usize;
            let symbol_addr = resolve_symbol_addr_cached(
                elf,
                runtime_base,
                &layout.sections,
                &symbols,
                symbol_names,
                symbol_index,
                relocation.relocation_type(),
                policy,
                &mut resolved_symbols,
            )?;
            let got_entry_addr = layout
                .got_entries
                .get(symbol_index)
                .and_then(|entry| *entry)
                .map(|offset| {
                    runtime_base
                        .checked_add(offset)
                        .ok_or("module GOT address overflow")
                })
                .transpose()?;
            apply_relocation(
                write_ptr,
                write_runtime_addr,
                relocation.relocation_type(),
                symbol_addr,
                addend,
                got_entry_addr,
            )?;
        }
    }

    Ok(())
}

fn relocation_target_is_loaded(
    section_headers: &[ModuleSectionHeader],
    reloc_header: ModuleSectionHeader,
) -> Result<bool, &'static str> {
    let target_index =
        usize::try_from(reloc_header.info).map_err(|_| "module relocation target overflow")?;
    relocation_target_index_is_loaded(section_headers, target_index)
}

fn relocation_target_index_is_loaded(
    section_headers: &[ModuleSectionHeader],
    target_index: usize,
) -> Result<bool, &'static str> {
    let Some(target) = section_headers.get(target_index) else {
        return Err("module relocation target section is out of range");
    };
    Ok(target.flags & u64::from(objelf::SHF_ALLOC) != 0 && target.size != 0)
}

fn resolve_symbol_addr_cached(
    elf: &ModuleElf<'_>,
    runtime_base: usize,
    layouts: &[Option<ModuleSectionLayout>],
    symbols: &[ModuleSymbol],
    symbol_names: &[u8],
    symbol_index: usize,
    relocation_type: u32,
    policy: SymbolResolvePolicy,
    cache: &mut [SymbolResolveCacheEntry],
) -> Result<usize, &'static str> {
    let symbol = symbols
        .get(symbol_index)
        .ok_or("module relocation symbol is out of range")?;
    let flavor = symbol_resolve_flavor(symbol, relocation_type);
    let slot = cache
        .get_mut(symbol_index)
        .ok_or("module relocation symbol is out of range")?;
    if let Some(cached) = slot[flavor.index()] {
        return cached;
    }

    let resolved = resolve_symbol_addr_with_names(
        elf,
        runtime_base,
        layouts,
        symbol,
        symbol_names,
        relocation_type,
        policy,
    );
    slot[flavor.index()] = Some(resolved);
    resolved
}

fn apply_relocation(
    write_ptr: *mut u8,
    write_runtime_addr: usize,
    relocation_type: u32,
    symbol_addr: usize,
    addend: i64,
    got_entry_addr: Option<usize>,
) -> Result<(), &'static str> {
    const R_X86_64_NONE: u32 = 0;
    const R_X86_64_GOTPCREL: u32 = 9;

    let symbol_value = add_signed_usize(symbol_addr, addend)?;
    match relocation_type {
        R_X86_64_NONE => Ok(()),
        R_X86_64_64 => {
            unsafe {
                (write_ptr as *mut u64).write_unaligned(symbol_value as u64);
            }
            Ok(())
        }
        R_X86_64_PC32 | R_X86_64_PLT32 => {
            let relative = (symbol_value as i128) - (write_runtime_addr as i128);
            let value = i32::try_from(relative).map_err(|_| "module PC32 relocation overflow")?;
            unsafe {
                (write_ptr as *mut i32).write_unaligned(value);
            }
            Ok(())
        }
        R_X86_64_GOTPCREL => {
            let got_entry_addr = got_entry_addr.ok_or("module GOT relocation target is missing")?;
            unsafe {
                (got_entry_addr as *mut u64).write_unaligned(symbol_addr as u64);
            }
            let got_value = add_signed_usize(got_entry_addr, addend)?;
            let relative = (got_value as i128) - (write_runtime_addr as i128);
            let value =
                i32::try_from(relative).map_err(|_| "module GOTPCREL relocation overflow")?;
            unsafe {
                (write_ptr as *mut i32).write_unaligned(value);
            }
            Ok(())
        }
        R_X86_64_32 => {
            let value =
                u32::try_from(symbol_value).map_err(|_| "module 32-bit relocation overflow")?;
            unsafe {
                (write_ptr as *mut u32).write_unaligned(value);
            }
            Ok(())
        }
        R_X86_64_32S => {
            let value = i32::try_from(symbol_value as i128)
                .map_err(|_| "module 32S relocation overflow")?;
            unsafe {
                (write_ptr as *mut i32).write_unaligned(value);
            }
            Ok(())
        }
        R_X86_64_PC64 => {
            let relative = (symbol_value as i128) - (write_runtime_addr as i128);
            let value = i64::try_from(relative).map_err(|_| "module PC64 relocation overflow")?;
            unsafe {
                (write_ptr as *mut i64).write_unaligned(value);
            }
            Ok(())
        }
        _ => Err("unsupported module relocation type"),
    }
}

fn relocation_write_size(relocation_type: u32) -> Result<usize, &'static str> {
    const R_X86_64_NONE: u32 = 0;
    const R_X86_64_GOTPCREL: u32 = 9;

    match relocation_type {
        R_X86_64_NONE => Ok(0),
        R_X86_64_64 | R_X86_64_PC64 => Ok(size_of::<u64>()),
        R_X86_64_PC32 | R_X86_64_PLT32 | R_X86_64_GOTPCREL | R_X86_64_32 | R_X86_64_32S => {
            Ok(size_of::<u32>())
        }
        _ => Err("unsupported module relocation type"),
    }
}

fn resolve_named_symbol_addr(
    elf: &ModuleElf<'_>,
    runtime_base: usize,
    layouts: &[Option<ModuleSectionLayout>],
    name: &str,
    expected_type: ModuleSymbolType,
) -> Result<usize, &'static str> {
    let (_, symbol) = find_symbol(elf, name)?.ok_or("driver module symbol is missing")?;
    if symbol.symbol_type()? != expected_type {
        return Err("driver module symbol type is invalid");
    }
    resolve_symbol_addr(
        elf,
        runtime_base,
        layouts,
        &symbol,
        R_X86_64_64,
        SymbolResolvePolicy::kernel_internal(),
    )
}

fn resolve_symbol_addr(
    elf: &ModuleElf<'_>,
    runtime_base: usize,
    layouts: &[Option<ModuleSectionLayout>],
    symbol: &ModuleSymbol,
    relocation_type: u32,
    policy: SymbolResolvePolicy,
) -> Result<usize, &'static str> {
    let symbol_names = symbol_string_table(elf)?;
    resolve_symbol_addr_with_names(
        elf,
        runtime_base,
        layouts,
        symbol,
        symbol_names,
        relocation_type,
        policy,
    )
}

fn resolve_symbol_addr_with_names(
    elf: &ModuleElf<'_>,
    runtime_base: usize,
    layouts: &[Option<ModuleSectionLayout>],
    symbol: &ModuleSymbol,
    symbol_names: &[u8],
    relocation_type: u32,
    policy: SymbolResolvePolicy,
) -> Result<usize, &'static str> {
    match symbol.shndx {
        objelf::SHN_UNDEF => resolve_external_symbol_for_relocation_with_names(
            elf,
            symbol,
            symbol_names,
            relocation_type,
            policy,
        ),
        objelf::SHN_ABS => {
            usize::try_from(symbol.value).map_err(|_| "absolute module symbol value overflow")
        }
        objelf::SHN_COMMON | objelf::SHN_XINDEX => {
            Err("module symbol section index is unsupported")
        }
        section_index => {
            let index = usize::from(section_index);
            let layout = layouts
                .get(index)
                .and_then(|entry| *entry)
                .ok_or("module symbol section is not loaded")?;
            let section_base = runtime_base
                .checked_add(layout.runtime_offset)
                .ok_or("module symbol address overflow")?;
            let offset =
                usize::try_from(symbol.value).map_err(|_| "module symbol value overflow")?;
            if offset > layout.size {
                return Err("module symbol is outside section");
            }
            if symbol.size != 0 {
                let symbol_size =
                    usize::try_from(symbol.size).map_err(|_| "module symbol size overflow")?;
                let symbol_end = offset
                    .checked_add(symbol_size)
                    .ok_or("module symbol range overflow")?;
                if symbol_end > layout.size {
                    return Err("module symbol range is outside section");
                }
            }
            section_base
                .checked_add(offset)
                .ok_or("module symbol address overflow")
        }
    }
}

fn symbol_resolve_flavor(symbol: &ModuleSymbol, relocation_type: u32) -> SymbolResolveFlavor {
    if symbol.shndx != objelf::SHN_UNDEF {
        return SymbolResolveFlavor::Direct;
    }

    if relocation_type == R_X86_64_PLT32 {
        return SymbolResolveFlavor::Trampoline;
    }

    if matches!(
        relocation_type,
        R_X86_64_PC32 | R_X86_64_32 | R_X86_64_32S | R_X86_64_PC64
    ) {
        return SymbolResolveFlavor::LowAlias;
    }

    SymbolResolveFlavor::Direct
}

fn resolve_external_symbol_by_name(
    symbol: &ModuleSymbol,
    name: &str,
    policy: SymbolResolvePolicy,
) -> Result<usize, &'static str> {
    if symbol.binding()? == ModuleSymbolBinding::Weak {
        if let Some(address) = resolve_allowed_external_symbol(name, policy) {
            return Ok(address);
        }
        if is_optional_weak_symbol(name) {
            driver_diag!(
                crate::debug::LogLevel::Warn,
                25,
                0,
                "driver module unresolved optional weak external: symbol={}",
                name
            );
            return Ok(0);
        }
        driver_diag!(
            crate::debug::LogLevel::Warn,
            26,
            0,
            "driver module unresolved weak external: symbol={}",
            name
        );
        return Err("module references unsupported weak external symbol");
    }

    let address = resolve_allowed_external_symbol(name, policy);
    let Some(address) = address else {
        if policy.is_linux_compat() && !is_allowed_linux_external_symbol(name, policy) {
            crate::debug::write_debugcon_only_parts_line(&[
                b"driver module disallowed external: module=",
                policy.module_name.as_bytes(),
                b" symbol=",
                name.as_bytes(),
                b" class=",
                class::name(policy.class).as_bytes(),
                b" bus=",
                bus::name(policy.bus).as_bytes(),
            ]);
            driver_diag!(
                crate::debug::LogLevel::Warn,
                27,
                0,
                "driver module disallowed external: module={} symbol={} class={} bus={}",
                policy.module_name,
                name,
                class::name(policy.class),
                bus::name(policy.bus)
            );
            return Err("module references disallowed external symbol");
        }
        crate::debug::write_debugcon_only_parts_line(&[
            b"driver module unresolved external: module=",
            policy.module_name.as_bytes(),
            b" symbol=",
            name.as_bytes(),
        ]);
        driver_diag!(
            crate::debug::LogLevel::Warn,
            28,
            0,
            "driver module unresolved external: symbol={} type={:?} binding={:?}",
            name,
            symbol.symbol_type().ok(),
            symbol.binding().ok()
        );
        return Err("module references unsupported external symbol");
    };
    Ok(address)
}

fn resolve_external_symbol_for_relocation_with_names(
    _elf: &ModuleElf<'_>,
    symbol: &ModuleSymbol,
    symbol_names: &[u8],
    relocation_type: u32,
    policy: SymbolResolvePolicy,
) -> Result<usize, &'static str> {
    if symbol.name == 0 {
        if symbol.binding()? == ModuleSymbolBinding::Weak {
            return Ok(0);
        }
        return Err("module references unnamed external symbol");
    }
    let name = symbol_name_from_table(symbol_names, symbol)
        .map_err(|_| "module external symbol name is invalid")?;
    let resolved = resolve_external_symbol_by_name(symbol, name, policy)?;
    if relocation_type == R_X86_64_PLT32 {
        return compat_trampoline_addr(resolved, compat_trampoline_mode_for_symbol(name));
    }

    if matches!(
        relocation_type,
        R_X86_64_PC32 | R_X86_64_32 | R_X86_64_32S | R_X86_64_PC64
    ) {
        return Ok(crate::memory::paging::lower_half_addr(resolved as u64) as usize);
    }

    Ok(resolved)
}

fn compat_trampoline_mode_for_symbol(name: &str) -> CompatTrampolineMode {
    match super::linux::export_abi(name) {
        super::linux::LinuxCompatExportAbi::AlignRustCall => CompatTrampolineMode::AlignRustCall,
        super::linux::LinuxCompatExportAbi::PreserveStackTail => {
            CompatTrampolineMode::PreserveStackTail
        }
    }
}

fn resolve_allowed_external_symbol(name: &str, policy: SymbolResolvePolicy) -> Option<usize> {
    if !policy.is_linux_compat() {
        return export::resolve_symbol(name);
    }

    resolve_linux_allowed_symbol(name, policy)
}

fn resolve_linux_allowed_symbol(name: &str, policy: SymbolResolvePolicy) -> Option<usize> {
    if let Some(address) = super::export::resolve_symbol(name) {
        return Some(address);
    }

    let common = super::linux::compiler::resolve_symbol(name)
        .or_else(|| super::linux::base::resolve_symbol(name))
        .or_else(|| super::linux::runtime::resolve_symbol(name))
        .or_else(|| super::linux::device::resolve_symbol(name))
        .or_else(|| super::linux::aux::resolve_symbol(name))
        .or_else(|| super::linux::export::resolve_symbol(name))
        .or_else(|| super::linux::dma::resolve_symbol(name))
        .or_else(|| super::linux::workqueue::resolve_symbol(name))
        .or_else(|| super::linux::irq::resolve_symbol(name))
        .or_else(|| super::linux::mmio::resolve_symbol(name))
        .or_else(|| super::linux::pci::resolve_symbol(name))
        .or_else(|| super::linux::input::resolve_symbol(name))
        .or_else(|| module_registry::resolve_symbol(name));

    if common.is_some() {
        return common;
    }

    match (policy.class, policy.bus) {
        (DriverClass::Input, DriverBus::Usb) => super::linux::hid::resolve_symbol(name)
            .or_else(|| super::linux::usb::resolve_symbol(name)),
        (DriverClass::Input, DriverBus::Serio) => super::linux::serio::resolve_symbol(name)
            .or_else(|| super::linux::ps2::resolve_symbol(name)),
        (DriverClass::Network, DriverBus::Virtio)
        | (DriverClass::Network, DriverBus::Pci) => None,
        (DriverClass::Usb, DriverBus::Pci) | (DriverClass::Usb, DriverBus::Platform) => {
            super::linux::usb::resolve_symbol(name)
                .or_else(|| super::linux::pci::resolve_symbol(name))
        }
        (DriverClass::Storage, DriverBus::Pci) => super::linux::pci::resolve_symbol(name),
        _ => None,
    }
}

fn is_allowed_linux_external_symbol(name: &str, policy: SymbolResolvePolicy) -> bool {
    if module_registry::resolve_symbol(name).is_some() {
        return true;
    }

    let common_allowed = super::linux::compiler::resolve_symbol(name).is_some()
        || super::linux::base::resolve_symbol(name).is_some()
        || super::linux::runtime::resolve_symbol(name).is_some()
        || super::linux::device::resolve_symbol(name).is_some()
        || super::linux::aux::resolve_symbol(name).is_some()
        || super::linux::export::resolve_symbol(name).is_some()
        || super::linux::dma::resolve_symbol(name).is_some()
        || super::linux::workqueue::resolve_symbol(name).is_some()
        || super::linux::irq::resolve_symbol(name).is_some()
        || super::linux::mmio::resolve_symbol(name).is_some()
        || super::linux::pci::resolve_symbol(name).is_some()
        || super::linux::input::resolve_symbol(name).is_some();
    if common_allowed {
        return true;
    }

    match (policy.class, policy.bus) {
        (DriverClass::Input, DriverBus::Usb) => {
            super::linux::hid::resolve_symbol(name).is_some()
                || super::linux::usb::resolve_symbol(name).is_some()
        }
        (DriverClass::Input, DriverBus::Serio) => {
            super::linux::serio::resolve_symbol(name).is_some()
                || super::linux::ps2::resolve_symbol(name).is_some()
        }
        (DriverClass::Network, DriverBus::Virtio)
        | (DriverClass::Network, DriverBus::Pci) => false,
        (DriverClass::Usb, DriverBus::Pci) | (DriverClass::Usb, DriverBus::Platform) => {
            super::linux::usb::resolve_symbol(name).is_some()
                || super::linux::pci::resolve_symbol(name).is_some()
        }
        (DriverClass::Storage, DriverBus::Pci) => super::linux::pci::resolve_symbol(name).is_some(),
        _ => false,
    }
}

fn is_optional_weak_symbol(name: &str) -> bool {
    matches!(
        name,
        "__fentry__"
            | "mcount"
            | "__x86_return_thunk"
            | "__stack_chk_guard"
            | "__this_module"
            | "_GLOBAL_OFFSET_TABLE_"
    )
}

fn compat_trampoline_addr(
    target_addr: usize,
    mode: CompatTrampolineMode,
) -> Result<usize, &'static str> {
    {
        let trampolines = KERNEL_COMPAT_TRAMPOLINES.lock();
        if let Some(entry) = trampolines
            .iter()
            .find(|entry| entry.target_addr == target_addr && entry.mode == mode)
        {
            return Ok(entry.runtime_addr);
        }
    }

    if KERNEL_COMPAT_TRAMPOLINES.lock().len() >= MAX_COMPAT_TRAMPOLINES {
        return Err("compat trampoline hard cap exceeded");
    }

    let (host_ptr, runtime_addr) = {
        let mut pages = KERNEL_COMPAT_TRAMPOLINE_PAGES.lock();
        if let Some(page) = pages.iter_mut().find(|page| page.has_free_slot()) {
            page.prepare_for_write()?;
            page.allocate_slot()
                .ok_or("compat trampoline page allocation failed")?
        } else {
            let mut page = KernelCompatTrampolinePage::new()?;
            page.prepare_for_write()?;
            let slot = page
                .allocate_slot()
                .ok_or("compat trampoline page allocation failed")?;
            pages.push(page);
            slot
        }
    };

    unsafe {
        let code = host_ptr;
        match mode {
            CompatTrampolineMode::AlignRustCall => {
                // Linux modules are built for an 8-byte stack boundary, while
                // RustOS handlers follow the normal x86-64 Rust/SysV call
                // alignment. For handlers without stack-passed arguments,
                // move the module return address into a stack slot, preserve
                // Linux callee-saved registers at the ABI boundary, keep the
                // restore pointer in callee-saved r12, align the call into Rust,
                // then return through the original module return address.
                //
                // pop r10
                // push r10
                // push rbx
                // push rbp
                // push r12
                // push r13
                // push r14
                // push r15
                // mov r12, rsp
                // and rsp, -16
                // movabs rax, imm64
                // call rax
                // mov rsp, r12
                // pop r15
                // pop r14
                // pop r13
                // pop r12
                // pop rbp
                // pop rbx
                // ret
                code.add(0).write(0x41);
                code.add(1).write(0x5A);
                code.add(2).write(0x41);
                code.add(3).write(0x52);
                code.add(4).write(0x53);
                code.add(5).write(0x55);
                code.add(6).write(0x41);
                code.add(7).write(0x54);
                code.add(8).write(0x41);
                code.add(9).write(0x55);
                code.add(10).write(0x41);
                code.add(11).write(0x56);
                code.add(12).write(0x41);
                code.add(13).write(0x57);
                code.add(14).write(0x49);
                code.add(15).write(0x89);
                code.add(16).write(0xE4);
                code.add(17).write(0x48);
                code.add(18).write(0x83);
                code.add(19).write(0xE4);
                code.add(20).write(0xF0);
                code.add(21).write(0x48);
                code.add(22).write(0xB8);
                (code.add(23) as *mut u64).write_unaligned(target_addr as u64);
                code.add(31).write(0xFF);
                code.add(32).write(0xD0);
                code.add(33).write(0x4C);
                code.add(34).write(0x89);
                code.add(35).write(0xE4);
                code.add(36).write(0x41);
                code.add(37).write(0x5F);
                code.add(38).write(0x41);
                code.add(39).write(0x5E);
                code.add(40).write(0x41);
                code.add(41).write(0x5D);
                code.add(42).write(0x41);
                code.add(43).write(0x5C);
                code.add(44).write(0x5D);
                code.add(45).write(0x5B);
                code.add(46).write(0xC3);
                for offset in 47..TRAMPOLINE_SIZE {
                    code.add(offset).write(0x90);
                }
            }
            CompatTrampolineMode::PreserveStackTail => {
                // Tail-jump into RustOS' resolved handler. This preserves the
                // Linux module's original return address and stack-passed
                // arguments exactly for varargs or >6-argument helpers.
                //
                // movabs r11, imm64
                // jmp r11
                code.add(0).write(0x49);
                code.add(1).write(0xBB);
                (code.add(2) as *mut u64).write_unaligned(target_addr as u64);
                code.add(10).write(0x41);
                code.add(11).write(0xFF);
                code.add(12).write(0xE3);
                for offset in 13..TRAMPOLINE_SIZE {
                    code.add(offset).write(0x90);
                }
            }
        }
    }

    {
        let mut pages = KERNEL_COMPAT_TRAMPOLINE_PAGES.lock();
        let Some(page) = pages.iter_mut().find(|page| {
            let start = page.allocation.host_base() as usize;
            let end = start + MODULE_PAGE_SIZE;
            (start..end).contains(&(host_ptr as usize))
        }) else {
            return Err("compat trampoline page disappeared");
        };
        page.seal_executable()?;
    }

    let mut trampolines = KERNEL_COMPAT_TRAMPOLINES.lock();
    if let Some(entry) = trampolines
        .iter()
        .find(|entry| entry.target_addr == target_addr && entry.mode == mode)
    {
        return Ok(entry.runtime_addr);
    }

    trampolines.push(KernelCompatTrampoline {
        target_addr,
        mode,
        runtime_addr,
    });
    Ok(runtime_addr)
}

fn find_named_section<'a>(
    elf: &'a ModuleElf<'a>,
    expected_name: &str,
) -> Result<Option<ModuleSectionHeader>, &'static str> {
    let names = section_header_string_table(elf)?;
    for section in section_header_entries(elf)? {
        if section.section_type == 0 {
            continue;
        }

        let name = read_string_table_entry(
            names,
            section.name,
            "module section name offset is out of range",
            "module section name is not UTF-8",
        )?;
        if name == expected_name {
            return Ok(Some(section));
        }
    }

    Ok(None)
}

fn symbol_string_table<'a>(elf: &'a ModuleElf<'a>) -> Result<&'a [u8], &'static str> {
    let section =
        find_named_section(elf, ".symtab")?.ok_or("module ELF does not contain .symtab")?;
    let string_table_index = usize::try_from(section.link)
        .map_err(|_| "module ELF symtab string table index overflow")?;
    let sections = section_header_entries(elf)?;
    let string_table = sections
        .get(string_table_index)
        .copied()
        .ok_or("module ELF symtab string table is invalid")?;
    if string_table.section_type != objelf::SHT_STRTAB {
        return Err("module ELF symtab string table format is invalid");
    }

    section_data_bytes_raw(
        elf,
        &string_table,
        "module ELF symtab string table is invalid",
    )
}

fn symbol_name_from_table<'a>(
    string_table: &'a [u8],
    symbol: &ModuleSymbol,
) -> Result<&'a str, &'static str> {
    read_string_table_entry(
        string_table,
        symbol.name,
        "module symbol name offset is out of range",
        "module symbol name is not UTF-8",
    )
}

fn symbol_table_entries(elf: &ModuleElf<'_>) -> Result<Vec<ModuleSymbol>, &'static str> {
    let section =
        find_named_section(elf, ".symtab")?.ok_or("module ELF does not contain .symtab")?;
    if section.section_type != objelf::SHT_SYMTAB {
        return Err("module ELF .symtab format is invalid");
    }

    let bytes = section_data_bytes_raw(elf, &section, "module ELF symtab could not be parsed")?;
    let entries = parse_fixed_size_table::<RawSym<LittleEndian>>(
        bytes,
        section.entry_size as usize,
        "module ELF .symtab format is invalid",
    )?;
    if entries.len() > MAX_MODULE_SYMBOLS {
        return Err("module symbol count exceeds hard cap");
    }
    let entries = entries
        .into_iter()
        .map(|raw| ModuleSymbol {
            name: raw.st_name.get(ELF_ENDIAN),
            info: raw.st_info,
            shndx: raw.st_shndx.get(ELF_ENDIAN),
            value: raw.st_value.get(ELF_ENDIAN),
            size: raw.st_size.get(ELF_ENDIAN),
        })
        .collect::<Vec<_>>();
    Ok(entries)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align <= 1 {
        return Some(value);
    }
    let mask = align.checked_sub(1)?;
    value.checked_add(mask).map(|value| value & !mask)
}

fn reserve_module_arena_bytes(bytes: usize) -> Result<(), &'static str> {
    let mut current = MODULE_ARENA_BYTES.load(Ordering::Acquire);
    loop {
        let next = current
            .checked_add(bytes)
            .ok_or("module arena accounting overflow")?;
        if next > MAX_MODULE_ARENA_BYTES {
            return Err("module arena hard size cap exceeded");
        }
        match MODULE_ARENA_BYTES.compare_exchange(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(()),
            Err(observed) => current = observed,
        }
    }
}

fn allocate_module_arena(size: usize, align: usize) -> Result<ModuleArenaAllocation, &'static str> {
    let align = align.max(MODULE_PAGE_SIZE);
    let raw_len = size
        .checked_add(align.saturating_sub(MODULE_PAGE_SIZE))
        .and_then(|value| align_up(value, MODULE_PAGE_SIZE))
        .ok_or("module allocation size overflow")?;
    reserve_module_arena_bytes(raw_len)?;

    let page_count = raw_len / MODULE_PAGE_SIZE;
    let phys_start = match crate::memory::phys::alloc_contiguous(page_count) {
        Some(phys) => phys,
        None => {
            MODULE_ARENA_BYTES.fetch_sub(raw_len, Ordering::AcqRel);
            return Err("module allocation failed");
        }
    };
    let raw_host = crate::memory::paging::higher_half_addr(phys_start.as_u64()) as usize;
    let aligned_host = match align_up(raw_host, align) {
        Some(value) => value,
        None => {
            for page_index in 0..page_count {
                let phys = phys_start
                    .as_u64()
                    .checked_add((page_index * MODULE_PAGE_SIZE) as u64)
                    .ok_or("module allocation size overflow")?;
                crate::memory::phys::free_frame(PhysAddr::new(phys));
            }
            MODULE_ARENA_BYTES.fetch_sub(raw_len, Ordering::AcqRel);
            return Err("module allocation alignment overflow");
        }
    };
    let offset = aligned_host
        .checked_sub(raw_host)
        .ok_or("module allocation alignment overflow")?;
    if offset.checked_add(size).is_none_or(|end| end > raw_len) {
        for page_index in 0..page_count {
            let phys = phys_start
                .as_u64()
                .checked_add((page_index * MODULE_PAGE_SIZE) as u64)
                .ok_or("module allocation size overflow")?;
            crate::memory::phys::free_frame(PhysAddr::new(phys));
        }
        MODULE_ARENA_BYTES.fetch_sub(raw_len, Ordering::AcqRel);
        return Err("module allocation alignment is invalid");
    }

    let raw_host_ptr = NonNull::new(raw_host as *mut u8).ok_or("module allocation failed")?;
    let host_ptr = NonNull::new(aligned_host as *mut u8).ok_or("module allocation failed")?;
    unsafe {
        ptr::write_bytes(raw_host_ptr.as_ptr(), 0, raw_len);
    }

    Ok(ModuleArenaAllocation {
        phys_start,
        page_count,
        raw_host_ptr,
        host_ptr,
        host_offset: offset,
        raw_len,
    })
}

fn should_skip_relocation_section(name: &str) -> bool {
    matches!(
        name,
        ".rela.altinstr_replacement"
            | ".rela.altinstr_aux"
            | ".rela.altinstructions"
            | ".rela__mcount_loc"
            | ".rela__param"
            | ".rela.retpoline_sites"
            | ".rela.return_sites"
            | ".rela.call_sites"
            | ".rela.smp_locks"
            | ".rela__bug_table"
            | ".rela__jump_table"
            | ".rela__patchable_function_entries"
            | ".rela__dyndbg"
    )
}

fn validate_module_exports(
    elf: &ModuleElf<'_>,
    header_symbol: Option<(usize, ModuleSymbol)>,
) -> Result<(), &'static str> {
    let header_symbol = if let Some(header_symbol) = header_symbol {
        Some(header_symbol)
    } else {
        find_symbol(elf, RUSTOS_DRIVER_HEADER_SYMBOL)?
    };
    let abi_symbol = find_symbol(elf, RUSTOS_DRIVER_ABI_VERSION_SYMBOL)?;
    let init_symbol = find_symbol(elf, RUSTOS_DRIVER_INIT_SYMBOL)?;

    let Some((_, header_entry)) = header_symbol else {
        return Err("driver module header symbol is missing");
    };
    if header_entry.symbol_type()? != ModuleSymbolType::Object {
        return Err("driver module header symbol type is invalid");
    }

    let Some((_, abi_entry)) = abi_symbol else {
        return Err("driver module ABI version symbol is missing");
    };
    if abi_entry.symbol_type()? != ModuleSymbolType::Func {
        return Err("driver module ABI version symbol type is invalid");
    }

    let Some((_, init_entry)) = init_symbol else {
        return Err("driver module init symbol is missing");
    };
    if init_entry.symbol_type()? != ModuleSymbolType::Func {
        return Err("driver module init symbol type is invalid");
    }

    Ok(())
}

fn ensure_unique_critical_symbols(elf: &ModuleElf<'_>) -> Result<(), &'static str> {
    ensure_unique_symbol(
        elf,
        RUSTOS_DRIVER_HEADER_SYMBOL,
        "driver module header symbol is duplicated",
    )?;
    ensure_unique_symbol(
        elf,
        RUSTOS_DRIVER_ABI_VERSION_SYMBOL,
        "driver module ABI version symbol is duplicated",
    )?;
    ensure_unique_symbol(
        elf,
        RUSTOS_DRIVER_INIT_SYMBOL,
        "driver module init symbol is duplicated",
    )?;
    ensure_unique_symbol(
        elf,
        LINUX_COMPAT_INIT_SYMBOL,
        "driver module linux init symbol is duplicated",
    )?;
    Ok(())
}

fn ensure_unique_symbol(
    elf: &ModuleElf<'_>,
    name: &str,
    duplicate_error: &'static str,
) -> Result<(), &'static str> {
    if symbol_match_count(elf, name)? > 1 {
        return Err(duplicate_error);
    }
    Ok(())
}

fn extract_module_header(elf: &ModuleElf<'_>) -> Result<DriverModuleHeader, &'static str> {
    let Some((_, entry)) = find_symbol(elf, RUSTOS_DRIVER_HEADER_SYMBOL)? else {
        return Err("driver module header symbol is missing");
    };

    if entry.size < size_of::<DriverModuleHeader>() as u64 {
        return Err("driver module header symbol is truncated");
    }
    if !matches!(
        entry.binding(),
        Ok(ModuleSymbolBinding::Global) | Ok(ModuleSymbolBinding::Weak)
    ) {
        return Err("driver module header symbol binding is invalid");
    }

    let section = symbol_section_header(elf, &entry, "driver module header section is invalid")?;
    let section_data =
        section_data_bytes_raw(elf, &section, "driver module header section is invalid")?;
    let start = usize::try_from(entry.value).map_err(|_| "driver module header range overflow")?;
    let end = start
        .checked_add(size_of::<DriverModuleHeader>())
        .ok_or("driver module header range overflow")?;
    if end > section_data.len() {
        return Err("driver module header range is outside section");
    }

    Ok(unsafe { (section_data.as_ptr().add(start) as *const DriverModuleHeader).read_unaligned() })
}

fn find_symbol<'a>(
    elf: &'a ModuleElf<'a>,
    name: &str,
) -> Result<Option<(usize, ModuleSymbol)>, &'static str> {
    let entries = symbol_table_entries(elf)?;
    let string_table = symbol_string_table(elf)?;

    for (index, entry) in entries.into_iter().enumerate() {
        if symbol_name_from_table(string_table, &entry)? == name {
            return Ok(Some((index, entry)));
        }
    }

    Ok(None)
}

fn symbol_match_count(elf: &ModuleElf<'_>, name: &str) -> Result<usize, &'static str> {
    let entries = symbol_table_entries(elf)?;
    let string_table = symbol_string_table(elf)?;
    let mut count = 0usize;
    for entry in entries {
        if symbol_name_from_table(string_table, &entry)? == name {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn section_data_bytes_raw<'a>(
    elf: &'a ModuleElf<'a>,
    section: &ModuleSectionHeader,
    error: &'static str,
) -> Result<&'a [u8], &'static str> {
    let start = usize::try_from(section.offset).map_err(|_| error)?;
    let size = usize::try_from(section.size).map_err(|_| error)?;
    let end = start.checked_add(size).ok_or(error)?;
    elf.input.get(start..end).ok_or(error)
}

fn module_elf_header(elf: &ModuleElf<'_>) -> Result<RawElfHeader<LittleEndian>, &'static str> {
    Ok(elf.header)
}

fn read_raw_elf_header(image: &[u8]) -> Result<RawElfHeader<LittleEndian>, &'static str> {
    let bytes: &[u8; size_of::<RawElfHeader<LittleEndian>>()] = image
        .get(..size_of::<RawElfHeader<LittleEndian>>())
        .ok_or("module ELF header is invalid")?
        .try_into()
        .map_err(|_| "module ELF header is invalid")?;
    Ok(unsafe { (bytes.as_ptr() as *const RawElfHeader<LittleEndian>).read_unaligned() })
}

fn parse_fixed_size_table<T: Copy>(
    bytes: &[u8],
    entry_size: usize,
    error: &'static str,
) -> Result<Vec<T>, &'static str> {
    let expected_size = size_of::<T>();
    let entry_size = match entry_size {
        0 => expected_size,
        value if value == expected_size => value,
        _ => return Err(error),
    };
    if bytes.len() % entry_size != 0 {
        return Err(error);
    }

    let mut entries = Vec::with_capacity(bytes.len() / entry_size);
    for chunk in bytes.chunks_exact(entry_size) {
        let entry = unsafe { (chunk.as_ptr() as *const T).read_unaligned() };
        entries.push(entry);
    }
    Ok(entries)
}

fn relocation_entries_by_header(
    elf: &ModuleElf<'_>,
    section: ModuleSectionHeader,
) -> Result<Vec<ModuleRela>, &'static str> {
    if section.section_type != objelf::SHT_RELA {
        return Err("module relocation table format is invalid");
    }

    let bytes =
        section_data_bytes_raw(elf, &section, "module relocation table could not be parsed")?;
    let entries = parse_fixed_size_table::<RawRela<LittleEndian>>(
        bytes,
        section.entry_size as usize,
        "module relocation table format is invalid",
    )?;
    if entries.len() > MAX_MODULE_RELOCATIONS {
        return Err("module relocation count exceeds hard cap");
    }
    Ok(entries
        .into_iter()
        .map(|raw| ModuleRela {
            offset: raw.r_offset.get(ELF_ENDIAN),
            info: raw.r_info.get(ELF_ENDIAN),
            addend: raw.r_addend.get(ELF_ENDIAN),
        })
        .collect())
}

fn symbol_section_header<'a>(
    elf: &'a ModuleElf<'a>,
    symbol: &ModuleSymbol,
    error: &'static str,
) -> Result<ModuleSectionHeader, &'static str> {
    match symbol.shndx {
        objelf::SHN_UNDEF | objelf::SHN_ABS | objelf::SHN_COMMON | objelf::SHN_XINDEX => Err(error),
        section_index => section_header_entries(elf)?
            .get(usize::from(section_index))
            .copied()
            .ok_or(error),
    }
}

pub(super) fn section_header_entries(
    elf: &ModuleElf<'_>,
) -> Result<Vec<ModuleSectionHeader>, &'static str> {
    let header = module_elf_header(elf)?;
    let start = usize::try_from(header.e_shoff.get(ELF_ENDIAN))
        .map_err(|_| "module ELF section table is invalid")?;
    let entry_size = usize::from(header.e_shentsize.get(ELF_ENDIAN));
    let count = usize::from(header.e_shnum.get(ELF_ENDIAN));
    if count > MAX_MODULE_SECTIONS {
        return Err("module section count exceeds hard cap");
    }
    if entry_size == 0 {
        return Err("module ELF section table is invalid");
    }
    let size = entry_size
        .checked_mul(count)
        .ok_or("module ELF section table is invalid")?;
    let end = start
        .checked_add(size)
        .ok_or("module ELF section table is invalid")?;
    let bytes = elf
        .input
        .get(start..end)
        .ok_or("module ELF section table is invalid")?;
    let entries = parse_fixed_size_table::<RawSectionHeader<LittleEndian>>(
        bytes,
        entry_size,
        "module ELF section table is invalid",
    )?;
    if entries.iter().any(|entry| {
        entry.sh_addralign.get(ELF_ENDIAN) != 0
            && usize::try_from(entry.sh_addralign.get(ELF_ENDIAN))
                .map_or(true, |align| align > MAX_MODULE_SECTION_ALIGN)
    }) {
        return Err("module section alignment exceeds hard cap");
    }
    let entries = entries
        .into_iter()
        .map(|raw| ModuleSectionHeader {
            name: raw.sh_name.get(ELF_ENDIAN),
            section_type: raw.sh_type.get(ELF_ENDIAN),
            flags: raw.sh_flags.get(ELF_ENDIAN),
            offset: raw.sh_offset.get(ELF_ENDIAN),
            size: raw.sh_size.get(ELF_ENDIAN),
            link: raw.sh_link.get(ELF_ENDIAN),
            info: raw.sh_info.get(ELF_ENDIAN),
            align: raw.sh_addralign.get(ELF_ENDIAN),
            entry_size: raw.sh_entsize.get(ELF_ENDIAN),
        })
        .collect::<Vec<_>>();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{
        ModuleMemoryClass, ModuleSectionHeader, PendingModuleExecGuard, classify_module_section,
        reset_for_tests, runtime_executable_addr_is_known, validate_module_path,
    };
    use object::elf as objelf;

    #[test]
    fn pending_module_exec_range_is_visible_during_initialization_only() {
        reset_for_tests();
        let start = 0x0012_6ed0usize;
        let size = 0x3000usize;
        assert!(!runtime_executable_addr_is_known(start + 0x40));

        {
            let _guard = PendingModuleExecGuard::new(start, size);
            assert!(runtime_executable_addr_is_known(start));
            assert!(runtime_executable_addr_is_known(start + 0x2fff));
            assert!(!runtime_executable_addr_is_known(start + size));
        }

        assert!(!runtime_executable_addr_is_known(start + 0x40));
        reset_for_tests();
    }

    #[test]
    fn rejects_noncanonical_module_paths() {
        assert!(validate_module_path("system/drivers/input/usbhid.ko").is_ok());
        assert_eq!(
            validate_module_path("../system/drivers/input/usbhid.ko"),
            Err("driver module path is invalid")
        );
        assert_eq!(
            validate_module_path("system/drivers//input/usbhid.ko"),
            Err("driver module path is invalid")
        );
        assert_eq!(
            validate_module_path("/system/drivers/input/usbhid.ko"),
            Err("driver module path is invalid")
        );
    }

    fn alloc_section(flags: u64) -> ModuleSectionHeader {
        ModuleSectionHeader {
            name: 0,
            section_type: objelf::SHT_PROGBITS,
            flags: flags | u64::from(objelf::SHF_ALLOC),
            offset: 0,
            size: 64,
            link: 0,
            info: 0,
            align: 8,
            entry_size: 0,
        }
    }

    #[test]
    fn classifies_linux_ro_after_init_sections() {
        let section = alloc_section(u64::from(objelf::SHF_WRITE));
        assert_eq!(
            classify_module_section(section, "__jump_table"),
            Ok(ModuleMemoryClass::CoreRoAfterInit)
        );
        assert_eq!(
            classify_module_section(section, ".static_call_sites"),
            Ok(ModuleMemoryClass::CoreRoAfterInit)
        );
        assert_eq!(
            classify_module_section(section, ".data..ro_after_init"),
            Ok(ModuleMemoryClass::CoreRoAfterInit)
        );
    }

    #[test]
    fn classifies_relro_and_init_sections() {
        let writable = alloc_section(u64::from(objelf::SHF_WRITE));
        assert_eq!(
            classify_module_section(writable, ".data.rel.ro.foo"),
            Ok(ModuleMemoryClass::CoreRodata)
        );
        assert_eq!(
            classify_module_section(writable, ".init.data.rel.ro.foo"),
            Ok(ModuleMemoryClass::InitRodata)
        );
        assert_eq!(
            classify_module_section(writable, ".exit.data"),
            Ok(ModuleMemoryClass::InitData)
        );
    }

    #[test]
    fn rejects_write_exec_sections() {
        let write_exec =
            alloc_section(u64::from(objelf::SHF_WRITE) | u64::from(objelf::SHF_EXECINSTR));
        assert_eq!(
            classify_module_section(write_exec, ".text.bad"),
            Err("module alloc section has invalid WRITE|EXEC flags")
        );
    }
}
// RING3-MIGRATION-REFERENCE END: Linux .ko module loader compatibility substrate exception.
