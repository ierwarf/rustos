use core::mem;

const SAVED_GPR_BYTES: usize = 15 * 8;
const SAVED_XMM_BYTES: usize = 16 * 16;
const CONTEXT_PREFIX_BYTES: usize = SAVED_GPR_BYTES + SAVED_XMM_BYTES; // 0x178
const IRET_FRAME_BYTES: usize = 5 * 8;
pub(super) const SAVED_CONTEXT_BYTES: usize = CONTEXT_PREFIX_BYTES + IRET_FRAME_BYTES; // 0x1a0

const _: [(); 0x78] = [(); SAVED_GPR_BYTES];
const _: [(); 0x100] = [(); SAVED_XMM_BYTES];
const _: [(); 0x178] = [(); CONTEXT_PREFIX_BYTES];
const _: [(); 0x28] = [(); IRET_FRAME_BYTES];
const _: [(); 0x1a0] = [(); SAVED_CONTEXT_BYTES];

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct SavedContext {
    pub(super) rax: u64,
    pub(super) rbx: u64,
    pub(super) rcx: u64,
    pub(super) rdx: u64,
    pub(super) rsi: u64,
    pub(super) rdi: u64,
    pub(super) rbp: u64,
    pub(super) r8: u64,
    pub(super) r9: u64,
    pub(super) r10: u64,
    pub(super) r11: u64,
    pub(super) r12: u64,
    pub(super) r13: u64,
    pub(super) r14: u64,
    pub(super) r15: u64,
    pub(super) xmm: [[u8; 16]; 16],
    pub(super) rsp: u64,
    pub(super) ss: u64,
    pub(super) rip: u64,
    pub(super) cs: u64,
    pub(super) rflags: u64,
}

const _: [(); 0x78] = [(); mem::offset_of!(SavedContext, xmm)];
const _: [(); 0x178] = [(); mem::offset_of!(SavedContext, rsp)];
const _: [(); 0x180] = [(); mem::offset_of!(SavedContext, ss)];
const _: [(); 0x188] = [(); mem::offset_of!(SavedContext, rip)];
const _: [(); 0x190] = [(); mem::offset_of!(SavedContext, cs)];
const _: [(); 0x198] = [(); mem::offset_of!(SavedContext, rflags)];
const _: [(); 0x1a0] = [(); mem::size_of::<SavedContext>()];
