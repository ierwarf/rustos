//! x86_64 privilege-segment and kernel-entry-stack construction.
//!
//! - **Owner:** `kernel-hal` owns per-CPU GDT/TSS publication.
//! - **Boundary:** Selectors and stack bounds become CPU privilege-transition
//!   authority.
//! - **Lifecycle:** A dense logical CPU slot is built completely before
//!   `lgdt`/TSS load makes it live; its TSS stacks remain CPU-private.
//! - **Concurrency:** Each CPU initializes and mutates only its own slot with
//!   interrupts excluded; release publication prevents partial observation.
//! - **Failure:** Invalid layout or stack topology is a boot-fatal contract
//!   violation.
//! - **Forbidden:** No shared TSS/RSP0/IST stack, reused user stack, raw APIC
//!   indexing, mutable live descriptor, or AP dispatch before this slot is live.
//! - **Evidence:** `exception-retirement` and `cpu-online-lifecycle`.
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::{PrivilegeLevel, VirtAddr};

use super::acpi::MAX_SUPPORTED_CPUS;

const KERNEL_PRIVILEGE_STACK_SIZE: usize = 256 * 1024;
const DOUBLE_FAULT_STACK_SIZE: usize = 128 * 1024;
const NMI_STACK_SIZE: usize = 64 * 1024;
pub(crate) const DOUBLE_FAULT_IST_INDEX: u16 = 0;
pub(crate) const NMI_IST_INDEX: u16 = 1;
const GDT_EMPTY: u8 = 0;
const GDT_BUILDING: u8 = 1;
const GDT_LIVE: u8 = 2;

#[repr(align(16))]
struct PrivilegeStack {
    _bytes: [u8; KERNEL_PRIVILEGE_STACK_SIZE],
}

#[repr(align(16))]
struct InterruptStack<const SIZE: usize> {
    _bytes: [u8; SIZE],
}

struct PrivilegeStackMemory(UnsafeCell<PrivilegeStack>);

// SAFETY: each array element has one permanent dense logical-CPU owner.
// Only that CPU mutates the contained privilege stack during private setup.
unsafe impl Sync for PrivilegeStackMemory {}

struct InterruptStackMemory<const SIZE: usize>(UnsafeCell<InterruptStack<SIZE>>);

// SAFETY: each array element has one permanent dense logical-CPU owner.
// Hardware and setup code access only the slot admitted for the current CPU.
unsafe impl<const SIZE: usize> Sync for InterruptStackMemory<SIZE> {}

struct TssMemory(UnsafeCell<TaskStateSegment>);

// SAFETY: each TSS slot is initialized and subsequently loaded only by its
// permanent dense logical-CPU owner before that CPU becomes Online.
unsafe impl Sync for TssMemory {}

static RING0_STACKS: [PrivilegeStackMemory; MAX_SUPPORTED_CPUS] = [const {
    PrivilegeStackMemory(UnsafeCell::new(PrivilegeStack {
        _bytes: [0; KERNEL_PRIVILEGE_STACK_SIZE],
    }))
}; MAX_SUPPORTED_CPUS];
static DOUBLE_FAULT_STACKS: [InterruptStackMemory<DOUBLE_FAULT_STACK_SIZE>; MAX_SUPPORTED_CPUS] = [const {
    InterruptStackMemory(UnsafeCell::new(InterruptStack {
        _bytes: [0; DOUBLE_FAULT_STACK_SIZE],
    }))
};
    MAX_SUPPORTED_CPUS];
static NMI_STACKS: [InterruptStackMemory<NMI_STACK_SIZE>; MAX_SUPPORTED_CPUS] = [const {
    InterruptStackMemory(UnsafeCell::new(InterruptStack {
        _bytes: [0; NMI_STACK_SIZE],
    }))
};
    MAX_SUPPORTED_CPUS];
static TSS_SLOTS: [TssMemory; MAX_SUPPORTED_CPUS] =
    [const { TssMemory(UnsafeCell::new(TaskStateSegment::new())) }; MAX_SUPPORTED_CPUS];

#[derive(Clone, Copy)]
struct Selectors {
    kernel_code: SegmentSelector,
    kernel_data: SegmentSelector,
    user_code: SegmentSelector,
    user_data: SegmentSelector,
    tss: SegmentSelector,
}

struct CpuGdtSlot {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<(GlobalDescriptorTable, Selectors)>>,
}

// SAFETY: each dense logical CPU initializes and loads only its own slot.
// Readers access slot zero only after its Release publication.
unsafe impl Sync for CpuGdtSlot {}

impl CpuGdtSlot {
    const fn empty() -> Self {
        Self {
            state: AtomicU8::new(GDT_EMPTY),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

static GDT_SLOTS: [CpuGdtSlot; MAX_SUPPORTED_CPUS] =
    [const { CpuGdtSlot::empty() }; MAX_SUPPORTED_CPUS];

pub fn init() {
    init_for_cpu(nucleus_core::util::lockdep::current_cpu_index());
}

pub fn init_for_cpu(logical_index: usize) {
    use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};

    assert!(
        logical_index < MAX_SUPPORTED_CPUS,
        "GDT invariant: logical CPU index exceeds capacity"
    );
    assert_eq!(
        logical_index,
        nucleus_core::util::lockdep::current_cpu_index(),
        "GDT invariant: CPU may initialize only its own descriptor slot"
    );
    let slot = &GDT_SLOTS[logical_index];
    // ORDERING: AcqRel claims the CPU-private descriptor build. Any prior state
    // means duplicate initialization or concurrent ownership.
    assert!(
        slot.state
            .compare_exchange(
                GDT_EMPTY,
                GDT_BUILDING,
                // ORDERING: AcqRel claims the unique initialization epoch.
                Ordering::AcqRel,
                // ORDERING: Acquire observes a competing initializer's state.
                Ordering::Acquire,
            )
            .is_ok(),
        "GDT invariant: logical CPU {logical_index} initialized twice"
    );

    set_privilege_stack_for_cpu(logical_index, default_ring0_stack_top(logical_index));
    set_interrupt_stack_for_cpu(
        logical_index,
        DOUBLE_FAULT_IST_INDEX,
        double_fault_stack_top(logical_index),
    );
    set_interrupt_stack_for_cpu(logical_index, NMI_IST_INDEX, nmi_stack_top(logical_index));

    let mut gdt = GlobalDescriptorTable::new();
    let kernel_code = gdt.append(Descriptor::kernel_code_segment());
    let kernel_data = gdt.append(Descriptor::kernel_data_segment());
    let user_data = gdt.append(Descriptor::user_data_segment());
    let user_code = gdt.append(Descriptor::user_code_segment());
    // SAFETY: this CPU's TSS slot is stable for the kernel lifetime and the
    // descriptor is not loaded until the complete GDT is written below.
    let tss = unsafe {
        gdt.append(Descriptor::tss_segment_unchecked(
            TSS_SLOTS[logical_index].0.get().cast_const(),
        ))
    };
    let expected = expected_selectors();
    assert_eq!(
        (kernel_code, kernel_data, user_data, user_code, tss),
        (
            expected.kernel_code,
            expected.kernel_data,
            expected.user_data,
            expected.user_code,
            expected.tss,
        ),
        "GDT invariant: descriptor append order changed selector ABI"
    );
    let value = (
        gdt,
        Selectors {
            kernel_code,
            kernel_data,
            user_code,
            user_data,
            tss,
        },
    );
    // SAFETY: the state claim gives this CPU exclusive initialization access.
    let live = unsafe { (&mut *slot.value.get()).write(value) };
    live.0.load();
    // SAFETY: selectors belong to the GDT just loaded on this CPU and the TSS
    // points to this CPU's private, initialized slot.
    unsafe {
        CS::set_reg(live.1.kernel_code);
        DS::set_reg(live.1.kernel_data);
        ES::set_reg(live.1.kernel_data);
        FS::set_reg(live.1.kernel_data);
        GS::set_reg(live.1.kernel_data);
        SS::set_reg(live.1.kernel_data);
        load_tss(live.1.tss);
    }
    // ORDERING: Release publishes the fully loaded CPU-private descriptor
    // state to selector users and the CPU-online lifecycle.
    slot.state.store(GDT_LIVE, Ordering::Release);
}

pub fn user_code_selector() -> SegmentSelector {
    expected_selectors().user_code
}

pub fn user_data_selector() -> SegmentSelector {
    expected_selectors().user_data
}

pub fn kernel_code_selector() -> SegmentSelector {
    expected_selectors().kernel_code
}

pub fn kernel_data_selector() -> SegmentSelector {
    expected_selectors().kernel_data
}

pub fn set_privilege_stack(stack_top: u64) {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    x86_64::instructions::interrupts::without_interrupts(|| {
        set_privilege_stack_for_cpu(cpu, stack_top);
    });
}

fn set_privilege_stack_for_cpu(logical_index: usize, stack_top: u64) {
    if stack_top == 0 {
        panic!("GDT invariant: ring0 privilege stack top must be non-zero");
    }
    assert_eq!(
        stack_top & 0xf,
        0,
        "GDT invariant: ring0 privilege stack top must be 16-byte aligned"
    );
    // SAFETY: the caller owns this logical CPU and either initializes its
    // unpublished TSS or excludes interrupts around the live RSP0 update.
    unsafe {
        (*TSS_SLOTS[logical_index].0.get()).privilege_stack_table[0] = VirtAddr::new(stack_top);
    }
}

/// The ring0 stack top this CPU's TSS currently publishes, or `0` if the CPU
/// index is outside the admitted topology.
///
/// Read back rather than tracked separately so the value is exactly what the
/// hardware would load on the next ring3 -> ring0 transition. The double-fault
/// handler uses it to tell a kernel stack overflow apart from every other
/// double fault, which is otherwise indistinguishable from the frame alone.
pub fn privilege_stack_top_for_current_cpu() -> u64 {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    if cpu >= MAX_SUPPORTED_CPUS {
        return 0;
    }
    // SAFETY: a read of one `u64`-sized field from this CPU's own permanently
    // owned TSS slot. The fatal path takes no lock and mutates nothing.
    unsafe { (*TSS_SLOTS[cpu].0.get()).privilege_stack_table[0].as_u64() }
}

pub fn set_interrupt_stack(index: u16, stack_top: u64) {
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    x86_64::instructions::interrupts::without_interrupts(|| {
        set_interrupt_stack_for_cpu(cpu, index, stack_top);
    });
}

fn set_interrupt_stack_for_cpu(logical_index: usize, index: u16, stack_top: u64) {
    if stack_top == 0 {
        panic!("GDT invariant: interrupt stack top must be non-zero");
    }

    let index = index as usize;
    if index >= 7 {
        panic!("GDT invariant: interrupt stack index out of range");
    }
    assert_eq!(
        stack_top & 0xf,
        0,
        "GDT invariant: interrupt stack top must be 16-byte aligned"
    );
    // SAFETY: the caller owns this CPU-private TSS and the target IST stack.
    unsafe {
        (*TSS_SLOTS[logical_index].0.get()).interrupt_stack_table[index] = VirtAddr::new(stack_top);
    }
}

const fn expected_selectors() -> Selectors {
    // Descriptor append order is an ABI: syscall/iret frame construction may
    // need selector values before the BSP has published its live GDT. The
    // initializer checks that the library assigned these exact indices.
    Selectors {
        kernel_code: SegmentSelector::new(1, PrivilegeLevel::Ring0),
        kernel_data: SegmentSelector::new(2, PrivilegeLevel::Ring0),
        user_data: SegmentSelector::new(3, PrivilegeLevel::Ring3),
        user_code: SegmentSelector::new(4, PrivilegeLevel::Ring3),
        tss: SegmentSelector::new(5, PrivilegeLevel::Ring0),
    }
}

fn default_ring0_stack_top(logical_index: usize) -> u64 {
    let base = RING0_STACKS[logical_index].0.get() as *const PrivilegeStack as u64;
    base + KERNEL_PRIVILEGE_STACK_SIZE as u64
}

fn double_fault_stack_top(logical_index: usize) -> u64 {
    let base = DOUBLE_FAULT_STACKS[logical_index].0.get()
        as *const InterruptStack<DOUBLE_FAULT_STACK_SIZE> as u64;
    base + DOUBLE_FAULT_STACK_SIZE as u64
}

fn nmi_stack_top(logical_index: usize) -> u64 {
    let base = NMI_STACKS[logical_index].0.get() as *const InterruptStack<NMI_STACK_SIZE> as u64;
    base + NMI_STACK_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::{
        DOUBLE_FAULT_IST_INDEX, MAX_SUPPORTED_CPUS, NMI_IST_INDEX, default_ring0_stack_top,
        double_fault_stack_top, nmi_stack_top,
    };

    #[test]
    fn per_cpu_privilege_and_ist_stacks_are_aligned_and_disjoint() {
        assert_ne!(NMI_IST_INDEX, DOUBLE_FAULT_IST_INDEX);
        let mut privilege = [0_u64; MAX_SUPPORTED_CPUS];
        let mut interrupt = [0_u64; MAX_SUPPORTED_CPUS];
        let mut nmi = [0_u64; MAX_SUPPORTED_CPUS];
        for cpu in 0..MAX_SUPPORTED_CPUS {
            privilege[cpu] = default_ring0_stack_top(cpu);
            interrupt[cpu] = double_fault_stack_top(cpu);
            nmi[cpu] = nmi_stack_top(cpu);
            assert_eq!(privilege[cpu] & 0xf, 0);
            assert_eq!(interrupt[cpu] & 0xf, 0);
            assert_eq!(nmi[cpu] & 0xf, 0);
            assert_ne!(privilege[cpu], interrupt[cpu]);
            assert_ne!(privilege[cpu], nmi[cpu]);
            assert_ne!(interrupt[cpu], nmi[cpu]);
            assert!(!privilege[..cpu].contains(&privilege[cpu]));
            assert!(!interrupt[..cpu].contains(&interrupt[cpu]));
            assert!(!nmi[..cpu].contains(&nmi[cpu]));
        }
    }
}
