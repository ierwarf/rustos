use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::str;

use spin::Mutex;
use xmas_elf::ElfFile;

use super::{ModuleLoadLayout, ModuleMemory};

const RELATIVE_EXPORT_ENTRY_SIZE: usize = 12;
const ABSOLUTE_EXPORT_ENTRY_SIZE: usize = 24;
const EXPORT_SECTION_NAMES: [&str; 2] = ["__ksymtab", "__ksymtab_gpl"];

struct ModuleExportRecord {
    owner: String,
    name: String,
    address: usize,
}

static MODULE_EXPORTS: Mutex<Vec<ModuleExportRecord>> = Mutex::new(Vec::new());

pub(super) fn resolve_symbol(name: &str) -> Option<usize> {
    let exports = MODULE_EXPORTS.lock();
    exports
        .iter()
        .find(|record| record.name == name)
        .map(|record| record.address)
}

pub(super) fn register_module_exports(
    module_name: &str,
    elf: &ElfFile<'_>,
    memory: &ModuleMemory,
    layout: &ModuleLoadLayout,
) -> Result<usize, &'static str> {
    let mut discovered = Vec::new();
    for section_name in EXPORT_SECTION_NAMES {
        discovered.extend(parse_export_section(
            module_name,
            section_name,
            elf,
            memory,
            layout,
        )?);
    }

    if discovered.is_empty() {
        return Ok(0);
    }

    let mut registered = 0usize;
    let mut registry = MODULE_EXPORTS.lock();
    for export in discovered {
        if let Some(existing) = registry
            .iter()
            .find(|existing| existing.name == export.name)
        {
            if existing.address != export.address || existing.owner != export.owner {
                crate::debug::println!(
                    "driver module export conflict: symbol={} owner={} existing_owner={}",
                    export.name,
                    export.owner,
                    existing.owner
                );
            }
            continue;
        }

        registry.push(export);
        registered += 1;
    }

    Ok(registered)
}

fn parse_export_section(
    module_name: &str,
    section_name: &str,
    elf: &ElfFile<'_>,
    memory: &ModuleMemory,
    layout: &ModuleLoadLayout,
) -> Result<Vec<ModuleExportRecord>, &'static str> {
    let Some((section_index, section_size, runtime_base, host_base)) =
        loaded_section_view(section_name, elf, memory, layout)?
    else {
        return Ok(Vec::new());
    };

    if section_size == 0 {
        return Ok(Vec::new());
    }

    if section_size % RELATIVE_EXPORT_ENTRY_SIZE == 0 {
        return parse_relative_export_entries(
            module_name,
            section_index,
            section_size,
            runtime_base,
            host_base,
            memory,
        );
    }

    if section_size % ABSOLUTE_EXPORT_ENTRY_SIZE == 0 {
        return parse_absolute_export_entries(module_name, section_size, host_base, memory);
    }

    crate::debug::println!(
        "driver module export table unsupported: module={} section={} size={}",
        module_name,
        section_name,
        section_size
    );
    Ok(Vec::new())
}

fn loaded_section_view(
    expected_name: &str,
    elf: &ElfFile<'_>,
    memory: &ModuleMemory,
    layout: &ModuleLoadLayout,
) -> Result<Option<(usize, usize, usize, *const u8)>, &'static str> {
    let section_names = super::section_header_string_table(elf)?;

    for (section_index, section) in elf.section_iter().enumerate() {
        if !section_name_matches(expected_name, section_names, &section)? {
            continue;
        }

        let section_size =
            usize::try_from(section.size()).map_err(|_| "module export section too large")?;
        let Some(section_layout) = layout.sections.get(section_index).and_then(|entry| *entry)
        else {
            return Err("module export section is not loaded");
        };
        let runtime_base = memory
            .runtime_base()
            .checked_add(section_layout.runtime_offset)
            .ok_or("module export section runtime address overflow")?;
        let host_base = unsafe { memory.host_base().add(section_layout.runtime_offset) };
        return Ok(Some((
            section_index,
            section_size,
            runtime_base,
            host_base.cast_const(),
        )));
    }

    Ok(None)
}

fn section_name_matches(
    expected_name: &str,
    section_names: &[u8],
    section: &xmas_elf::sections::SectionHeader<'_>,
) -> Result<bool, &'static str> {
    let name = super::read_string_table_entry(
        section_names,
        section.name(),
        "module section name offset is out of range",
        "module section name is not UTF-8",
    )?;
    Ok(name == expected_name)
}

fn parse_relative_export_entries(
    module_name: &str,
    _section_index: usize,
    section_size: usize,
    section_runtime_base: usize,
    section_host_base: *const u8,
    memory: &ModuleMemory,
) -> Result<Vec<ModuleExportRecord>, &'static str> {
    let mut exports = Vec::new();

    let mut entry_offset = 0usize;
    while entry_offset + RELATIVE_EXPORT_ENTRY_SIZE <= section_size {
        let value_rel = read_i32(section_host_base, entry_offset);
        let name_rel = read_i32(section_host_base, entry_offset + 4);
        let namespace_rel = read_i32(section_host_base, entry_offset + 8);

        let value_field_runtime = section_runtime_base
            .checked_add(entry_offset)
            .ok_or("module export value field address overflow")?;
        let name_field_runtime = value_field_runtime
            .checked_add(4)
            .ok_or("module export name field address overflow")?;
        let namespace_field_runtime = value_field_runtime
            .checked_add(8)
            .ok_or("module export namespace field address overflow")?;

        let value_addr = super::add_signed_usize(value_field_runtime, value_rel as i64)?;
        let name_addr = super::add_signed_usize(name_field_runtime, name_rel as i64)?;
        let namespace_addr =
            super::add_signed_usize(namespace_field_runtime, namespace_rel as i64)?;

        let name = read_runtime_c_string(memory, name_addr)?;
        if !name.is_empty() {
            if namespace_addr != namespace_field_runtime {
                let _ = read_runtime_c_string(memory, namespace_addr)?;
            }
            exports.push(ModuleExportRecord {
                owner: module_name.to_string(),
                name,
                address: value_addr,
            });
        }

        entry_offset += RELATIVE_EXPORT_ENTRY_SIZE;
    }

    Ok(exports)
}

fn parse_absolute_export_entries(
    module_name: &str,
    section_size: usize,
    section_host_base: *const u8,
    memory: &ModuleMemory,
) -> Result<Vec<ModuleExportRecord>, &'static str> {
    let mut exports = Vec::new();

    let mut entry_offset = 0usize;
    while entry_offset + ABSOLUTE_EXPORT_ENTRY_SIZE <= section_size {
        let value_addr = read_u64(section_host_base, entry_offset) as usize;
        let name_addr = read_u64(section_host_base, entry_offset + 8) as usize;
        let namespace_addr = read_u64(section_host_base, entry_offset + 16) as usize;

        let name = read_runtime_c_string(memory, name_addr)?;
        if !name.is_empty() {
            if namespace_addr != 0 {
                let _ = read_runtime_c_string(memory, namespace_addr)?;
            }
            exports.push(ModuleExportRecord {
                owner: module_name.to_string(),
                name,
                address: value_addr,
            });
        }

        entry_offset += ABSOLUTE_EXPORT_ENTRY_SIZE;
    }

    Ok(exports)
}

fn read_runtime_c_string(
    memory: &ModuleMemory,
    runtime_addr: usize,
) -> Result<String, &'static str> {
    let offset = runtime_to_host_offset(memory, runtime_addr)?;
    let bytes = unsafe {
        core::slice::from_raw_parts(
            memory.host_base().add(offset).cast_const(),
            memory.size - offset,
        )
    };
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("module export string is not terminated")?;
    let text = str::from_utf8(&bytes[..end]).map_err(|_| "module export string is not UTF-8")?;
    Ok(text.to_string())
}

fn runtime_to_host_offset(
    memory: &ModuleMemory,
    runtime_addr: usize,
) -> Result<usize, &'static str> {
    let offset = runtime_addr
        .checked_sub(memory.runtime_base())
        .ok_or("module export address is outside the loaded image")?;
    if offset >= memory.size {
        return Err("module export address is outside the loaded image");
    }
    Ok(offset)
}

fn read_i32(base: *const u8, offset: usize) -> i32 {
    unsafe { (base.add(offset) as *const i32).read_unaligned() }
}

fn read_u64(base: *const u8, offset: usize) -> u64 {
    unsafe { (base.add(offset) as *const u64).read_unaligned() }
}
