use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};

struct Selectors {
    kernel_code: SegmentSelector,
    kernel_data: SegmentSelector,
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());

        // Keep user segments allocated for future ring3 support.
        let _user_code = gdt.append(Descriptor::user_code_segment());
        let _user_data = gdt.append(Descriptor::user_data_segment());

        (
            gdt,
            Selectors {
                kernel_code,
                kernel_data,
            },
        )
    };
}

pub fn init() {
    use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};

    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        DS::set_reg(GDT.1.kernel_data);
        ES::set_reg(GDT.1.kernel_data);
        FS::set_reg(GDT.1.kernel_data);
        GS::set_reg(GDT.1.kernel_data);
        SS::set_reg(GDT.1.kernel_data);
    }
}
