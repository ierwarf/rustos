use core::arch::global_asm;

global_asm!(
    r#"
    .global __x86_indirect_thunk_rax
__x86_indirect_thunk_rax:
    jmp rax

    .global __x86_indirect_thunk_rdx
__x86_indirect_thunk_rdx:
    jmp rdx

    .global __x86_indirect_thunk_rcx
__x86_indirect_thunk_rcx:
    jmp rcx

    .global __x86_indirect_thunk_r9
__x86_indirect_thunk_r9:
    jmp r9

    .global __x86_indirect_thunk_r13
__x86_indirect_thunk_r13:
    jmp r13

    .global __x86_indirect_thunk_r15
__x86_indirect_thunk_r15:
    jmp r15

    .global __x86_return_thunk
__x86_return_thunk:
    ret
"#
);

unsafe extern "C" {
    fn __x86_indirect_thunk_rax();
    fn __x86_indirect_thunk_rdx();
    fn __x86_indirect_thunk_rcx();
    fn __x86_indirect_thunk_r9();
    fn __x86_indirect_thunk_r13();
    fn __x86_indirect_thunk_r15();
    fn __x86_return_thunk();
}

static REF_STACK_CHK_GUARD: u64 = 0x8d48_5a71_f2b3_c694;

pub(crate) unsafe extern "C" fn __fentry__() {}

pub(crate) unsafe extern "C" fn __dynamic_pr_debug() -> i32 {
    0
}

pub(crate) unsafe extern "C" fn __stack_chk_fail() -> ! {
    panic!("linux compat module triggered __stack_chk_fail");
}

pub(crate) unsafe extern "C" fn __fortify_panic(_msg: *const i8) -> ! {
    panic!("linux compat module triggered __fortify_panic");
}

pub(crate) unsafe extern "C" fn __ubsan_handle_load_invalid_value() {}

pub(crate) unsafe extern "C" fn __ubsan_handle_out_of_bounds() {}

pub(crate) unsafe extern "C" fn __ubsan_handle_shift_out_of_bounds() {}

pub(crate) fn init_cpu_local_symbols() {
    crate::user::syscall::set_linux_compat_stack_guard(REF_STACK_CHK_GUARD);
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "__fentry__" => Some(__fentry__ as *const () as usize),
        "__dynamic_pr_debug" => Some(__dynamic_pr_debug as *const () as usize),
        "__stack_chk_fail" => Some(__stack_chk_fail as *const () as usize),
        "__fortify_panic" => Some(__fortify_panic as *const () as usize),
        "__ref_stack_chk_guard" => Some(crate::user::syscall::linux_compat_stack_guard_offset()),
        "__ubsan_handle_load_invalid_value" => {
            Some(__ubsan_handle_load_invalid_value as *const () as usize)
        }
        "__ubsan_handle_out_of_bounds" => Some(__ubsan_handle_out_of_bounds as *const () as usize),
        "__ubsan_handle_shift_out_of_bounds" => {
            Some(__ubsan_handle_shift_out_of_bounds as *const () as usize)
        }
        "__x86_indirect_thunk_rax" => Some(__x86_indirect_thunk_rax as *const () as usize),
        "__x86_indirect_thunk_rdx" => Some(__x86_indirect_thunk_rdx as *const () as usize),
        "__x86_indirect_thunk_rcx" => Some(__x86_indirect_thunk_rcx as *const () as usize),
        "__x86_indirect_thunk_r9" => Some(__x86_indirect_thunk_r9 as *const () as usize),
        "__x86_indirect_thunk_r13" => Some(__x86_indirect_thunk_r13 as *const () as usize),
        "__x86_indirect_thunk_r15" => Some(__x86_indirect_thunk_r15 as *const () as usize),
        "__x86_return_thunk" => Some(__x86_return_thunk as *const () as usize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_symbol;

    #[test]
    fn stack_guard_uses_gs_relative_offset() {
        assert_eq!(
            resolve_symbol("__ref_stack_chk_guard"),
            Some(crate::user::syscall::linux_compat_stack_guard_offset())
        );
    }
}
