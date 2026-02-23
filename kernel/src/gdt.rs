use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};

struct Selectors {
    kernel_code_selector: SegmentSelector,
    kernel_data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code_selector = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data_selector = gdt.append(Descriptor::kernel_data_segment());

        let user_code_selector = gdt.append(Descriptor::user_code_segment());
        let user_data_selector = gdt.append(Descriptor::user_data_segment());

        (
            gdt,
            Selectors {
                kernel_code_selector,
                kernel_data_selector,
                user_code_selector,
                user_data_selector,
            },
        )
    };
}

pub fn init() {
    use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};

    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code_selector);
        DS::set_reg(GDT.1.kernel_data_selector);
        ES::set_reg(GDT.1.kernel_data_selector);
        FS::set_reg(GDT.1.kernel_data_selector);
        GS::set_reg(GDT.1.kernel_data_selector);
        SS::set_reg(GDT.1.kernel_data_selector);
    }
}
