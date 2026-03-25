use core::cell::UnsafeCell;

use lazy_static::lazy_static;
use x86_64::VirtAddr;
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

const KERNEL_PRIVILEGE_STACK_SIZE: usize = 256 * 1024;
const DOUBLE_FAULT_STACK_SIZE: usize = 128 * 1024;
pub(crate) const DOUBLE_FAULT_IST_INDEX: u16 = 0;

#[repr(align(16))]
struct PrivilegeStack {
    _bytes: [u8; KERNEL_PRIVILEGE_STACK_SIZE],
}

#[repr(align(16))]
struct InterruptStack<const SIZE: usize> {
    _bytes: [u8; SIZE],
}

struct PrivilegeStackMemory(UnsafeCell<PrivilegeStack>);

unsafe impl Sync for PrivilegeStackMemory {}

struct InterruptStackMemory<const SIZE: usize>(UnsafeCell<InterruptStack<SIZE>>);

unsafe impl<const SIZE: usize> Sync for InterruptStackMemory<SIZE> {}

struct TssMemory(UnsafeCell<TaskStateSegment>);

unsafe impl Sync for TssMemory {}

static RING0_STACK: PrivilegeStackMemory = PrivilegeStackMemory(UnsafeCell::new(PrivilegeStack {
    _bytes: [0; KERNEL_PRIVILEGE_STACK_SIZE],
}));
static DOUBLE_FAULT_STACK: InterruptStackMemory<DOUBLE_FAULT_STACK_SIZE> =
    InterruptStackMemory(UnsafeCell::new(InterruptStack {
        _bytes: [0; DOUBLE_FAULT_STACK_SIZE],
    }));
static TSS: TssMemory = TssMemory(UnsafeCell::new(TaskStateSegment::new()));

struct Selectors {
    kernel_code: SegmentSelector,
    kernel_data: SegmentSelector,
    user_code: SegmentSelector,
    user_data: SegmentSelector,
    tss: SegmentSelector,
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss =
            unsafe { gdt.append(Descriptor::tss_segment_unchecked(TSS.0.get().cast_const())) };

        (
            gdt,
            Selectors {
                kernel_code,
                kernel_data,
                user_code,
                user_data,
                tss,
            },
        )
    };
}

pub fn init() {
    use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};

    set_privilege_stack(default_ring0_stack_top());
    set_interrupt_stack(DOUBLE_FAULT_IST_INDEX, double_fault_stack_top());
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        DS::set_reg(GDT.1.kernel_data);
        ES::set_reg(GDT.1.kernel_data);
        FS::set_reg(GDT.1.kernel_data);
        GS::set_reg(GDT.1.kernel_data);
        SS::set_reg(GDT.1.kernel_data);
        load_tss(GDT.1.tss);
    }
}

pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code
}

pub fn user_data_selector() -> SegmentSelector {
    GDT.1.user_data
}

pub fn kernel_code_selector() -> SegmentSelector {
    GDT.1.kernel_code
}

pub fn kernel_data_selector() -> SegmentSelector {
    GDT.1.kernel_data
}

pub fn set_privilege_stack(stack_top: u64) {
    if stack_top == 0 {
        panic!("ring0 privilege stack top must be non-zero");
    }

    unsafe {
        (*TSS.0.get()).privilege_stack_table[0] = VirtAddr::new(stack_top);
    }
}

pub fn set_interrupt_stack(index: u16, stack_top: u64) {
    if stack_top == 0 {
        panic!("interrupt stack top must be non-zero");
    }

    let index = index as usize;
    if index >= 7 {
        panic!("interrupt stack index out of range");
    }

    unsafe {
        (*TSS.0.get()).interrupt_stack_table[index] = VirtAddr::new(stack_top);
    }
}

fn default_ring0_stack_top() -> u64 {
    let base = RING0_STACK.0.get() as *const PrivilegeStack as u64;
    base + KERNEL_PRIVILEGE_STACK_SIZE as u64
}

fn double_fault_stack_top() -> u64 {
    let base = DOUBLE_FAULT_STACK.0.get() as *const InterruptStack<DOUBLE_FAULT_STACK_SIZE> as u64;
    base + DOUBLE_FAULT_STACK_SIZE as u64
}
