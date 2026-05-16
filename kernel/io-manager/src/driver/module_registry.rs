use super::loader::{ModuleElf, ModuleLoadLayout, ModuleMemory};

pub(super) fn resolve_symbol(_name: &str) -> Option<usize> {
    None
}

pub(super) fn register_module_exports(
    _module_name: &str,
    _elf: &ModuleElf<'_>,
    _memory: &ModuleMemory,
    _layout: &ModuleLoadLayout,
) -> Result<usize, &'static str> {
    Ok(0)
}
