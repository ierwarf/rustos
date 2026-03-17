mod bus;
mod class;
mod export;
pub(crate) mod input;
pub(crate) mod linux;
pub(crate) mod serio;

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use core::ptr::{self, NonNull};

use driver_abi::{
    DriverBus, DriverClass, DriverInitFn, DriverKernelApiV1, DriverModuleHeader,
    RUSTOS_DRIVER_ABI_VERSION_SYMBOL, RUSTOS_DRIVER_HEADER_SYMBOL, RUSTOS_DRIVER_INIT_SYMBOL,
};
use spin::Mutex;
use xmas_elf::ElfFile;
use xmas_elf::header::{Machine, Type as ElfType};
use xmas_elf::sections::{SHF_ALLOC, SectionData, SectionHeader, ShType};
use xmas_elf::symbol_table::{Binding, Entry, Type as SymbolType};

const LINUX_COMPAT_INIT_SYMBOL: &str = "init_module";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DriverExecutionModel {
    KernelBuiltin,
    LoadableElf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DriverModuleState {
    Validated,
    Deferred,
    Loaded,
    LoadFailed,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DriverRecord {
    pub(crate) name: &'static str,
    pub(crate) class: DriverClass,
    pub(crate) bus: DriverBus,
    pub(crate) model: DriverExecutionModel,
    pub(crate) image_path: Option<&'static str>,
    pub(crate) module_state: Option<DriverModuleState>,
    pub(crate) module_header: Option<DriverModuleHeader>,
    pub(crate) validation_error: Option<&'static str>,
}

static DRIVER_REGISTRY: Mutex<Vec<DriverRecord>> = Mutex::new(Vec::new());
static LOADED_MODULES: Mutex<Vec<LoadedDriverModule>> = Mutex::new(Vec::new());
static KERNEL_COMPAT_TRAMPOLINES: Mutex<Vec<KernelCompatTrampoline>> = Mutex::new(Vec::new());
static DRIVER_KERNEL_API: DriverKernelApiV1 = DriverKernelApiV1::new(
    Some(serio::register_driver),
    Some(input::report_pointer_packet),
);

const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;
const R_X86_64_PC64: u32 = 24;

pub(crate) fn exported_kernel_api() -> *const DriverKernelApiV1 {
    &DRIVER_KERNEL_API
}

pub(crate) fn register_kernel_builtin(name: &'static str, class: DriverClass, bus: DriverBus) {
    debug_assert!(class::is_supported(class));
    debug_assert!(bus::is_supported(bus));

    let mut registry = DRIVER_REGISTRY.lock();
    if registry
        .iter()
        .any(|record| record.name == name && record.class == class && record.bus == bus)
    {
        return;
    }

    registry.push(DriverRecord {
        name,
        class,
        bus,
        model: DriverExecutionModel::KernelBuiltin,
        image_path: None,
        module_state: None,
        module_header: None,
        validation_error: None,
    });
}

pub(crate) fn register_loadable_elf(
    name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    image_path: &'static str,
) {
    let (module_state, module_header, validation_error) =
        match validate_module_image(image_path, name, class, bus) {
            Ok(header) => {
                crate::debug::println!(
                    "driver module validated: name={} class={} bus={} path={}",
                    name,
                    class::name(class),
                    bus::name(bus),
                    image_path
                );
                (Some(DriverModuleState::Validated), Some(header), None)
            }
            Err(error) => {
                crate::debug::println!(
                    "driver module validation failed: name={} class={} bus={} path={} error={}",
                    name,
                    class::name(class),
                    bus::name(bus),
                    image_path,
                    error
                );
                (Some(DriverModuleState::Invalid), None, Some(error))
            }
        };

    let mut registry = DRIVER_REGISTRY.lock();
    if registry.iter().any(|record| {
        record.name == name
            && record.class == class
            && record.bus == bus
            && record.model == DriverExecutionModel::LoadableElf
            && record.image_path == Some(image_path)
    }) {
        return;
    }

    registry.push(DriverRecord {
        name,
        class,
        bus,
        model: DriverExecutionModel::LoadableElf,
        image_path: Some(image_path),
        module_state,
        module_header,
        validation_error,
    });
}

pub(crate) fn initialize_loadable_modules() {
    let candidates = {
        let registry = DRIVER_REGISTRY.lock();
        let mut pending = Vec::new();
        for record in registry.iter() {
            if record.model != DriverExecutionModel::LoadableElf {
                continue;
            }
            if record.module_state != Some(DriverModuleState::Validated) {
                continue;
            }
            let Some(image_path) = record.image_path else {
                continue;
            };
            pending.push((record.name, record.class, record.bus, image_path));
        }
        pending
    };

    crate::debug::println!(
        "driver module initialization start: candidates={}",
        candidates.len()
    );

    for (name, class, bus, image_path) in candidates {
        match load_module_image(name, class, bus, image_path) {
            Ok(module) => {
                let mut registry = DRIVER_REGISTRY.lock();
                if let Some(record) = registry.iter_mut().find(|record| {
                    record.name == name
                        && record.model == DriverExecutionModel::LoadableElf
                        && record.image_path == Some(image_path)
                }) {
                    record.module_state = Some(DriverModuleState::Loaded);
                    record.validation_error = None;
                }
                drop(registry);

                crate::debug::println!(
                    "driver module loaded: name={} class={} bus={} path={} base={:#x} host={:#x}",
                    module.name,
                    class::name(class),
                    bus::name(bus),
                    module.image_path,
                    module.memory.runtime_base(),
                    module.memory.host_base() as usize
                );
                LOADED_MODULES.lock().push(module);
            }
            Err(error) => {
                if error == "module references unsupported external symbol" {
                    let mut registry = DRIVER_REGISTRY.lock();
                    if let Some(record) = registry.iter_mut().find(|record| {
                        record.name == name
                            && record.model == DriverExecutionModel::LoadableElf
                            && record.image_path == Some(image_path)
                    }) {
                        record.module_state = Some(DriverModuleState::Deferred);
                        record.validation_error = Some(error);
                    }
                    drop(registry);

                    crate::debug::println!(
                        "driver module deferred: name={} class={} bus={} path={} reason={}",
                        name,
                        class::name(class),
                        bus::name(bus),
                        image_path,
                        error
                    );
                    continue;
                }

                crate::debug::println!(
                    "driver module load failed: name={} class={} bus={} path={} error={}",
                    name,
                    class::name(class),
                    bus::name(bus),
                    image_path,
                    error
                );
                let mut registry = DRIVER_REGISTRY.lock();
                if let Some(record) = registry.iter_mut().find(|record| {
                    record.name == name
                        && record.model == DriverExecutionModel::LoadableElf
                        && record.image_path == Some(image_path)
                }) {
                    record.module_state = Some(DriverModuleState::LoadFailed);
                    record.validation_error = Some(error);
                }
            }
        }
    }

    let registry = DRIVER_REGISTRY.lock();
    for record in registry
        .iter()
        .filter(|record| record.model == DriverExecutionModel::LoadableElf)
    {
        crate::debug::println!(
            "driver module status: name={} class={} bus={} path={} state={:?} error={}",
            record.name,
            class::name(record.class),
            bus::name(record.bus),
            record.image_path.unwrap_or("-"),
            record.module_state,
            record.validation_error.unwrap_or("-")
        );
    }
}

#[cfg(test)]
pub(crate) fn snapshot_registered_drivers(dest: &mut [DriverRecord]) -> usize {
    let registry = DRIVER_REGISTRY.lock();
    let count = dest.len().min(registry.len());
    dest[..count].copy_from_slice(&registry[..count]);
    count
}

fn validate_module_image(
    image_path: &str,
    expected_name: &str,
    expected_class: DriverClass,
    expected_bus: DriverBus,
) -> Result<DriverModuleHeader, &'static str> {
    let image = crate::fat::read_file_to_vec(image_path).map_err(|_| "module image not found")?;
    let elf = ElfFile::new(image.as_slice()).map_err(|_| "module ELF is invalid")?;

    if elf.header.pt2.type_().as_type() != ElfType::Relocatable {
        return Err("module ELF is not relocatable");
    }
    if elf.header.pt2.machine().as_machine() != Machine::X86_64 {
        return Err("module ELF machine is not x86_64");
    }

    let abi = detect_module_abi(
        &elf,
        expected_name,
        expected_class,
        expected_bus,
        image_path,
    )?;
    let header = abi.header();

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

struct ModuleMemory {
    ptr: NonNull<u8>,
    size: usize,
    align: usize,
    runtime_base: usize,
}

impl ModuleMemory {
    fn host_base(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn runtime_base(&self) -> usize {
        self.runtime_base
    }
}

impl Drop for ModuleMemory {
    fn drop(&mut self) {
        if self.size == 0 {
            return;
        }
        let layout = Layout::from_size_align(self.size, self.align).expect("module memory layout");
        unsafe {
            dealloc(self.ptr.as_ptr(), layout);
        }
    }
}

unsafe impl Send for ModuleMemory {}

struct LoadedDriverModule {
    name: &'static str,
    image_path: &'static str,
    _header: DriverModuleHeader,
    memory: ModuleMemory,
}

unsafe impl Send for LoadedDriverModule {}

struct KernelCompatTrampoline {
    target_addr: usize,
    host_ptr: NonNull<u8>,
    size: usize,
    align: usize,
    runtime_addr: usize,
}

impl Drop for KernelCompatTrampoline {
    fn drop(&mut self) {
        if self.size == 0 {
            return;
        }

        let layout =
            Layout::from_size_align(self.size, self.align).expect("compat trampoline layout");
        unsafe {
            dealloc(self.host_ptr.as_ptr(), layout);
        }
    }
}

unsafe impl Send for KernelCompatTrampoline {}

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

#[derive(Clone, Copy)]
struct ModuleSectionLayout {
    runtime_offset: usize,
    size: usize,
}

struct ModuleLoadLayout {
    sections: Vec<Option<ModuleSectionLayout>>,
    got_entries: Vec<Option<usize>>,
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

fn load_module_image(
    name: &'static str,
    class: DriverClass,
    bus: DriverBus,
    image_path: &'static str,
) -> Result<LoadedDriverModule, &'static str> {
    let image = crate::fat::read_file_to_vec(image_path).map_err(|_| "module image not found")?;
    let elf = ElfFile::new(image.as_slice()).map_err(|_| "module ELF is invalid")?;
    let abi = detect_module_abi(&elf, name, class, bus, image_path)?;
    let header = abi.header();

    if let Some(symbol_name) = first_unsupported_external_symbol(&elf)? {
        crate::debug::println!(
            "driver module unresolved external: name={} path={} symbol={}",
            name,
            image_path,
            symbol_name
        );
        return Err("module references unsupported external symbol");
    }

    let (memory, layout) = allocate_module_memory(&elf)?;
    copy_alloc_sections(&elf, memory.host_base(), &layout.sections);
    apply_module_relocations(&elf, memory.host_base(), memory.runtime_base(), &layout)?;

    let status = match abi {
        ModuleAbi::RustOs(_) => {
            let init_addr = resolve_named_symbol_addr(
                &elf,
                memory.runtime_base(),
                &layout.sections,
                RUSTOS_DRIVER_INIT_SYMBOL,
                SymbolType::Func,
            )?;
            crate::debug::println!(
                "driver module init begin: name={} abi=rustos path={} base={:#x} host={:#x} entry={:#x}",
                name,
                image_path,
                memory.runtime_base(),
                memory.host_base() as usize,
                init_addr
            );
            let init: DriverInitFn = unsafe { core::mem::transmute(init_addr) };
            unsafe { init(exported_kernel_api()) }
        }
        ModuleAbi::LinuxCompat(_) => {
            let init_addr = resolve_named_symbol_addr(
                &elf,
                memory.runtime_base(),
                &layout.sections,
                LINUX_COMPAT_INIT_SYMBOL,
                SymbolType::Func,
            )?;
            crate::debug::println!(
                "driver module init begin: name={} abi=linux path={} base={:#x} host={:#x} entry={:#x}",
                name,
                image_path,
                memory.runtime_base(),
                memory.host_base() as usize,
                init_addr
            );
            let init: unsafe extern "C" fn() -> i32 = unsafe { core::mem::transmute(init_addr) };
            unsafe { init() }
        }
    };
    crate::debug::println!(
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

    Ok(LoadedDriverModule {
        name,
        image_path,
        _header: header,
        memory,
    })
}

fn detect_module_abi(
    elf: &ElfFile<'_>,
    expected_name: &str,
    expected_class: DriverClass,
    expected_bus: DriverBus,
    image_path: &str,
) -> Result<ModuleAbi, &'static str> {
    if validate_module_exports(elf).is_ok() {
        let header = extract_module_header(elf)?;
        return Ok(ModuleAbi::RustOs(header));
    }

    let Some((_, init_entry)) = find_symbol(elf, LINUX_COMPAT_INIT_SYMBOL)? else {
        return Err("driver module init symbol is missing");
    };
    if init_entry.get_type().ok() != Some(SymbolType::Func) {
        return Err("driver module init symbol type is invalid");
    }

    Ok(ModuleAbi::LinuxCompat(DriverModuleHeader::from_runtime(
        expected_class,
        expected_bus,
        image_path,
        expected_name,
    )))
}

fn allocate_module_memory(
    elf: &ElfFile<'_>,
) -> Result<(ModuleMemory, ModuleLoadLayout), &'static str> {
    const R_X86_64_GOTPCREL: u32 = 9;

    let mut sections = vec![None; elf.header.pt2.sh_count() as usize];
    let mut total_size = 0usize;
    let mut max_align = 1usize;

    for (index, section) in elf.section_iter().enumerate() {
        if section.flags() & SHF_ALLOC == 0 {
            continue;
        }
        let size = usize::try_from(section.size()).map_err(|_| "module section too large")?;
        if size == 0 {
            continue;
        }
        let align = usize::try_from(section.align())
            .map_err(|_| "module section alignment overflow")?
            .max(1);
        total_size = align_up(total_size, align).ok_or("module section layout overflow")?;
        sections[index] = Some(ModuleSectionLayout {
            runtime_offset: total_size,
            size,
        });
        total_size = total_size
            .checked_add(size)
            .ok_or("module image size overflow")?;
        max_align = max_align.max(align);
    }

    let symbols = symbol_table_entries(elf)?;
    let mut got_entries = vec![None; symbols.len()];
    for reloc_section in elf.section_iter() {
        if !matches!(reloc_section.get_type(), Ok(ShType::Rela)) {
            continue;
        }

        let SectionData::Rela64(relocations) = reloc_section
            .get_data(elf)
            .map_err(|_| "module relocation table could not be parsed")?
        else {
            return Err("module relocation table format is invalid");
        };

        for relocation in relocations.iter() {
            if relocation.get_type() != R_X86_64_GOTPCREL {
                continue;
            }
            let symbol_index = relocation.get_symbol_table_index() as usize;
            let Some(slot) = got_entries.get_mut(symbol_index) else {
                return Err("module relocation symbol is out of range");
            };
            if slot.is_some() {
                continue;
            }
            total_size =
                align_up(total_size, size_of::<u64>()).ok_or("module GOT layout overflow")?;
            *slot = Some(total_size);
            total_size = total_size
                .checked_add(size_of::<u64>())
                .ok_or("module GOT size overflow")?;
            max_align = max_align.max(size_of::<u64>());
        }
    }

    if total_size == 0 {
        return Err("module ELF contains no alloc sections");
    }

    let layout =
        Layout::from_size_align(total_size, max_align).map_err(|_| "module layout is invalid")?;
    let ptr = unsafe { alloc_zeroed(layout) };
    let ptr = NonNull::new(ptr).ok_or("module allocation failed")?;
    let runtime_base = crate::paging::lower_half_addr(ptr.as_ptr() as u64) as usize;

    Ok((
        ModuleMemory {
            ptr,
            size: total_size,
            align: max_align,
            runtime_base,
        },
        ModuleLoadLayout {
            sections,
            got_entries,
        },
    ))
}

fn copy_alloc_sections(elf: &ElfFile<'_>, base: *mut u8, layouts: &[Option<ModuleSectionLayout>]) {
    for (index, section) in elf.section_iter().enumerate() {
        let Some(layout) = layouts[index] else {
            continue;
        };

        let dest = unsafe { base.add(layout.runtime_offset) };
        if matches!(section.get_type(), Ok(ShType::NoBits)) {
            unsafe {
                ptr::write_bytes(dest, 0, layout.size);
            }
            continue;
        }

        let source = section.raw_data(elf);
        let copy_len = layout.size.min(source.len());
        unsafe {
            ptr::copy_nonoverlapping(source.as_ptr(), dest, copy_len);
        }
    }
}

fn apply_module_relocations(
    elf: &ElfFile<'_>,
    host_base: *mut u8,
    runtime_base: usize,
    layout: &ModuleLoadLayout,
) -> Result<(), &'static str> {
    let symbols = symbol_table_entries(elf)?;
    let mut resolved_symbols =
        vec![[None; SymbolResolveFlavor::COUNT]; symbols.len()];

    for reloc_section in elf.section_iter() {
        if !matches!(reloc_section.get_type(), Ok(ShType::Rela)) {
            continue;
        }

        let target_index = usize::try_from(reloc_section.info())
            .map_err(|_| "module relocation target overflow")?;
        let Some(target_layout) = layout.sections.get(target_index).and_then(|entry| *entry) else {
            continue;
        };

        let SectionData::Rela64(relocations) = reloc_section
            .get_data(elf)
            .map_err(|_| "module relocation table could not be parsed")?
        else {
            return Err("module relocation table format is invalid");
        };

        for relocation in relocations.iter() {
            let reloc_offset = usize::try_from(relocation.get_offset())
                .map_err(|_| "module relocation offset overflow")?;
            let write_ptr = unsafe { host_base.add(target_layout.runtime_offset) };
            let write_ptr = unsafe { write_ptr.add(reloc_offset) };
            let write_runtime_addr = runtime_base
                .checked_add(target_layout.runtime_offset)
                .and_then(|addr| addr.checked_add(reloc_offset))
                .ok_or("module relocation target overflow")?;
            let addend = relocation.get_addend() as i64;
            let symbol_index = relocation.get_symbol_table_index() as usize;
            let symbol_addr = resolve_symbol_addr_cached(
                elf,
                runtime_base,
                &layout.sections,
                symbols,
                symbol_index,
                relocation.get_type(),
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
                relocation.get_type(),
                symbol_addr,
                addend,
                got_entry_addr,
            )?;
        }
    }

    Ok(())
}

fn resolve_symbol_addr_cached(
    elf: &ElfFile<'_>,
    runtime_base: usize,
    layouts: &[Option<ModuleSectionLayout>],
    symbols: &[xmas_elf::symbol_table::Entry64],
    symbol_index: usize,
    relocation_type: u32,
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

    let resolved = resolve_symbol_addr(elf, runtime_base, layouts, symbol, relocation_type);
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

fn resolve_named_symbol_addr(
    elf: &ElfFile<'_>,
    runtime_base: usize,
    layouts: &[Option<ModuleSectionLayout>],
    name: &str,
    expected_type: SymbolType,
) -> Result<usize, &'static str> {
    let (symbol_index, symbol) =
        find_symbol(elf, name)?.ok_or("driver module symbol is missing")?;
    if symbol.get_type().ok() != Some(expected_type) {
        return Err("driver module symbol type is invalid");
    }
    resolve_symbol_addr(
        elf,
        runtime_base,
        layouts,
        symbol_table_entries(elf)?
            .get(symbol_index)
            .ok_or("driver module symbol index is out of range")?,
        R_X86_64_64,
    )
}

fn resolve_symbol_addr(
    elf: &ElfFile<'_>,
    runtime_base: usize,
    layouts: &[Option<ModuleSectionLayout>],
    symbol: &xmas_elf::symbol_table::Entry64,
    relocation_type: u32,
) -> Result<usize, &'static str> {
    match symbol.shndx() {
        xmas_elf::sections::SHN_UNDEF => {
            resolve_external_symbol_for_relocation(elf, symbol, relocation_type)
        }
        xmas_elf::sections::SHN_ABS => {
            usize::try_from(symbol.value()).map_err(|_| "absolute module symbol value overflow")
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
                usize::try_from(symbol.value()).map_err(|_| "module symbol value overflow")?;
            section_base
                .checked_add(offset)
                .ok_or("module symbol address overflow")
        }
    }
}

fn symbol_resolve_flavor(
    symbol: &xmas_elf::symbol_table::Entry64,
    relocation_type: u32,
) -> SymbolResolveFlavor {
    if symbol.shndx() != xmas_elf::sections::SHN_UNDEF {
        return SymbolResolveFlavor::Direct;
    }

    let is_function = symbol.get_type().ok() == Some(SymbolType::Func);
    let use_trampoline = match relocation_type {
        R_X86_64_PLT32 => true,
        R_X86_64_PC32 | R_X86_64_32 | R_X86_64_32S => is_function,
        _ => false,
    };
    if use_trampoline {
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

fn resolve_external_symbol(
    elf: &ElfFile<'_>,
    symbol: &xmas_elf::symbol_table::Entry64,
) -> Result<usize, &'static str> {
    if symbol.get_binding().ok() == Some(Binding::Weak) {
        if symbol.name() == 0 {
            return Ok(0);
        }
        if let Ok(name) = symbol.get_name(elf) {
            if let Some(address) = export::resolve_symbol(name) {
                return Ok(address);
            }
        }
        return Ok(0);
    }

    if symbol.name() == 0 {
        return Err("module references unnamed external symbol");
    }

    let name = symbol
        .get_name(elf)
        .map_err(|_| "module external symbol name is invalid")?;
    export::resolve_symbol(name).ok_or("module references unsupported external symbol")
}

fn resolve_external_symbol_for_relocation(
    elf: &ElfFile<'_>,
    symbol: &xmas_elf::symbol_table::Entry64,
    relocation_type: u32,
) -> Result<usize, &'static str> {
    let resolved = resolve_external_symbol(elf, symbol)?;
    let is_function = symbol.get_type().ok() == Some(SymbolType::Func);

    let use_trampoline = match relocation_type {
        R_X86_64_PLT32 => true,
        R_X86_64_PC32 | R_X86_64_32 | R_X86_64_32S => is_function,
        _ => false,
    };
    if use_trampoline {
        return compat_trampoline_addr(resolved);
    }

    let use_low_alias = matches!(
        relocation_type,
        R_X86_64_PC32 | R_X86_64_32 | R_X86_64_32S | R_X86_64_PC64
    );
    if use_low_alias {
        return Ok(crate::paging::lower_half_addr(resolved as u64) as usize);
    }

    Ok(resolved)
}

fn compat_trampoline_addr(target_addr: usize) -> Result<usize, &'static str> {
    {
        let trampolines = KERNEL_COMPAT_TRAMPOLINES.lock();
        if let Some(entry) = trampolines
            .iter()
            .find(|entry| entry.target_addr == target_addr)
        {
            return Ok(entry.runtime_addr);
        }
    }

    let layout = Layout::from_size_align(16, 16).map_err(|_| "compat trampoline layout")?;
    let host_ptr = unsafe { alloc_zeroed(layout) };
    let host_ptr = NonNull::new(host_ptr).ok_or("compat trampoline allocation failed")?;
    let runtime_addr = crate::paging::lower_half_addr(host_ptr.as_ptr() as u64) as usize;

    unsafe {
        let code = host_ptr.as_ptr();
        // movabs r11, imm64 ; jmp r11
        code.add(0).write(0x49);
        code.add(1).write(0xBB);
        (code.add(2) as *mut u64).write_unaligned(target_addr as u64);
        code.add(10).write(0x41);
        code.add(11).write(0xFF);
        code.add(12).write(0xE3);
        code.add(13).write(0x90);
        code.add(14).write(0x90);
        code.add(15).write(0x90);
    }

    let mut trampolines = KERNEL_COMPAT_TRAMPOLINES.lock();
    if let Some(entry) = trampolines
        .iter()
        .find(|entry| entry.target_addr == target_addr)
    {
        unsafe {
            dealloc(host_ptr.as_ptr(), layout);
        }
        return Ok(entry.runtime_addr);
    }

    trampolines.push(KernelCompatTrampoline {
        target_addr,
        host_ptr,
        size: 16,
        align: 16,
        runtime_addr,
    });
    Ok(runtime_addr)
}

fn read_string_table_entry<'a>(
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

fn section_header_string_table<'a>(elf: &'a ElfFile<'a>) -> Result<&'a [u8], &'static str> {
    let section = elf
        .section_header(elf.header.pt2.sh_str_index())
        .map_err(|_| "module ELF shstr section is invalid")?;
    if section
        .get_type()
        .map_err(|_| "module ELF shstr section is invalid")?
        != ShType::StrTab
    {
        return Err("module ELF shstr section format is invalid");
    }

    Ok(section.raw_data(elf))
}

fn find_named_section<'a>(
    elf: &'a ElfFile<'a>,
    expected_name: &str,
) -> Result<Option<SectionHeader<'a>>, &'static str> {
    let names = section_header_string_table(elf)?;
    for section in elf.section_iter() {
        if matches!(section.get_type(), Ok(ShType::Null)) {
            continue;
        }

        let name = read_string_table_entry(
            names,
            section.name(),
            "module section name offset is out of range",
            "module section name is not UTF-8",
        )?;
        if name == expected_name {
            return Ok(Some(section));
        }
    }

    Ok(None)
}

fn symbol_string_table<'a>(elf: &'a ElfFile<'a>) -> Result<&'a [u8], &'static str> {
    let section = find_named_section(elf, ".symtab")?.ok_or("module ELF does not contain .symtab")?;
    let string_table_index =
        u16::try_from(section.link()).map_err(|_| "module ELF symtab string table index overflow")?;
    let string_table = elf
        .section_header(string_table_index)
        .map_err(|_| "module ELF symtab string table is invalid")?;
    if string_table
        .get_type()
        .map_err(|_| "module ELF symtab string table is invalid")?
        != ShType::StrTab
    {
        return Err("module ELF symtab string table format is invalid");
    }

    Ok(string_table.raw_data(elf))
}

fn symbol_name_from_table<'a>(
    string_table: &'a [u8],
    symbol: &xmas_elf::symbol_table::Entry64,
) -> Result<&'a str, &'static str> {
    read_string_table_entry(
        string_table,
        symbol.name(),
        "module symbol name offset is out of range",
        "module symbol name is not UTF-8",
    )
}

fn symbol_table_entries<'a>(
    elf: &'a ElfFile<'a>,
) -> Result<&'a [xmas_elf::symbol_table::Entry64], &'static str> {
    let section = find_named_section(elf, ".symtab")?.ok_or("module ELF does not contain .symtab")?;
    let SectionData::SymbolTable64(entries) = section
        .get_data(elf)
        .map_err(|_| "module ELF symtab could not be parsed")?
    else {
        return Err("module ELF .symtab format is invalid");
    };
    Ok(entries)
}

fn first_unsupported_external_symbol<'a>(
    elf: &'a ElfFile<'a>,
) -> Result<Option<&'a str>, &'static str> {
    let string_table = symbol_string_table(elf)?;
    for symbol in symbol_table_entries(elf)?.iter() {
        if symbol.shndx() != xmas_elf::sections::SHN_UNDEF {
            continue;
        }
        if symbol.get_binding().ok() == Some(Binding::Weak) {
            continue;
        }
        if symbol.name() == 0 {
            continue;
        }
        let name = symbol_name_from_table(string_table, symbol)?;
        if export::resolve_symbol(name).is_none() {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align <= 1 {
        return Some(value);
    }
    let mask = align.checked_sub(1)?;
    value.checked_add(mask).map(|value| value & !mask)
}

fn add_signed_usize(value: usize, addend: i64) -> Result<usize, &'static str> {
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

fn validate_module_exports(elf: &ElfFile<'_>) -> Result<(), &'static str> {
    let header_symbol = find_symbol(elf, RUSTOS_DRIVER_HEADER_SYMBOL)?;
    let abi_symbol = find_symbol(elf, RUSTOS_DRIVER_ABI_VERSION_SYMBOL)?;
    let init_symbol = find_symbol(elf, RUSTOS_DRIVER_INIT_SYMBOL)?;

    let Some((_, header_entry)) = header_symbol else {
        return Err("driver module header symbol is missing");
    };
    if header_entry.get_type().ok() != Some(SymbolType::Object) {
        return Err("driver module header symbol type is invalid");
    }

    let Some((_, abi_entry)) = abi_symbol else {
        return Err("driver module ABI version symbol is missing");
    };
    if abi_entry.get_type().ok() != Some(SymbolType::Func) {
        return Err("driver module ABI version symbol type is invalid");
    }

    let Some((_, init_entry)) = init_symbol else {
        return Err("driver module init symbol is missing");
    };
    if init_entry.get_type().ok() != Some(SymbolType::Func) {
        return Err("driver module init symbol type is invalid");
    }

    Ok(())
}

fn extract_module_header(elf: &ElfFile<'_>) -> Result<DriverModuleHeader, &'static str> {
    let Some((symbol_index, entry)) = find_symbol(elf, RUSTOS_DRIVER_HEADER_SYMBOL)? else {
        return Err("driver module header symbol is missing");
    };

    if entry.size() < size_of::<DriverModuleHeader>() as u64 {
        return Err("driver module header symbol is truncated");
    }
    if !matches!(entry.get_binding(), Ok(Binding::Global) | Ok(Binding::Weak)) {
        return Err("driver module header symbol binding is invalid");
    }

    let section = entry
        .get_section_header(elf, symbol_index)
        .map_err(|_| "driver module header section is invalid")?;
    let section_data = section.raw_data(elf);
    let start = entry.value() as usize;
    let end = start
        .checked_add(size_of::<DriverModuleHeader>())
        .ok_or("driver module header range overflow")?;
    if end > section_data.len() {
        return Err("driver module header range is outside section");
    }

    Ok(unsafe { (section_data.as_ptr().add(start) as *const DriverModuleHeader).read_unaligned() })
}

fn find_symbol<'a>(
    elf: &'a ElfFile<'a>,
    name: &str,
) -> Result<Option<(usize, &'a xmas_elf::symbol_table::Entry64)>, &'static str> {
    let entries = symbol_table_entries(elf)?;
    let string_table = symbol_string_table(elf)?;

    for (index, entry) in entries.iter().enumerate() {
        if symbol_name_from_table(string_table, entry)? == name {
            return Ok(Some((index, entry)));
        }
    }

    Ok(None)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    DRIVER_REGISTRY.lock().clear();
}

#[cfg(test)]
mod tests {
    use super::{
        DriverBus, DriverClass, DriverExecutionModel, DriverModuleState, DriverRecord,
        register_kernel_builtin, register_loadable_elf, reset_for_tests,
        snapshot_registered_drivers,
    };

    #[test]
    fn snapshot_contains_registered_builtin_drivers() {
        reset_for_tests();
        register_kernel_builtin("uefi-gop", DriverClass::Display, DriverBus::Platform);
        register_kernel_builtin("legacy-keyboard", DriverClass::Input, DriverBus::Serio);

        let mut records = [DriverRecord {
            name: "",
            class: DriverClass::Display,
            bus: DriverBus::Platform,
            model: DriverExecutionModel::LoadableElf,
            image_path: None,
            module_state: None,
            module_header: None,
            validation_error: None,
        }; 4];
        let count = snapshot_registered_drivers(&mut records);

        assert_eq!(count, 2);
        assert_eq!(
            records[0],
            DriverRecord {
                name: "uefi-gop",
                class: DriverClass::Display,
                bus: DriverBus::Platform,
                model: DriverExecutionModel::KernelBuiltin,
                image_path: None,
                module_state: None,
                module_header: None,
                validation_error: None,
            }
        );
        assert_eq!(
            records[1],
            DriverRecord {
                name: "legacy-keyboard",
                class: DriverClass::Input,
                bus: DriverBus::Serio,
                model: DriverExecutionModel::KernelBuiltin,
                image_path: None,
                module_state: None,
                module_header: None,
                validation_error: None,
            }
        );
    }

    #[test]
    fn duplicate_registration_is_ignored() {
        reset_for_tests();
        register_kernel_builtin("legacy-keyboard", DriverClass::Input, DriverBus::Serio);
        register_kernel_builtin("legacy-keyboard", DriverClass::Input, DriverBus::Serio);

        let mut records = [DriverRecord {
            name: "",
            class: DriverClass::Display,
            bus: DriverBus::Platform,
            model: DriverExecutionModel::LoadableElf,
            image_path: None,
            module_state: None,
            module_header: None,
            validation_error: None,
        }; 2];
        let count = snapshot_registered_drivers(&mut records);

        assert_eq!(count, 1);
        assert_eq!(records[0].name, "legacy-keyboard");
    }

    #[test]
    fn missing_module_is_recorded_as_invalid() {
        reset_for_tests();
        register_loadable_elf(
            "missing",
            DriverClass::Input,
            DriverBus::Usb,
            "system/drivers/input/does-not-exist.ko",
        );

        let mut records = [DriverRecord {
            name: "",
            class: DriverClass::Display,
            bus: DriverBus::Platform,
            model: DriverExecutionModel::KernelBuiltin,
            image_path: None,
            module_state: None,
            module_header: None,
            validation_error: None,
        }; 1];
        let count = snapshot_registered_drivers(&mut records);

        assert_eq!(count, 1);
        assert_eq!(records[0].module_state, Some(DriverModuleState::Invalid));
        assert_eq!(records[0].validation_error, Some("module image not found"));
    }
}
