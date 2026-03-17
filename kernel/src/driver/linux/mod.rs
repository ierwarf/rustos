pub(crate) mod base;
pub(crate) mod compat;
pub(crate) mod compiler;
pub(crate) mod device;
pub(crate) mod export;
pub(crate) mod input;
pub(crate) mod ps2;
pub(crate) mod runtime;
pub(crate) mod serio;
pub(crate) mod workqueue;

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    compiler::resolve_symbol(name)
        .or_else(|| base::resolve_symbol(name))
        .or_else(|| runtime::resolve_symbol(name))
        .or_else(|| device::resolve_symbol(name))
        .or_else(|| export::resolve_symbol(name))
        .or_else(|| workqueue::resolve_symbol(name))
        .or_else(|| serio::resolve_symbol(name))
        .or_else(|| ps2::resolve_symbol(name))
        .or_else(|| input::resolve_symbol(name))
}
