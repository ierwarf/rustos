use std::fs;
use std::path::PathBuf;

mod shared {
    include!("../tools/build_log_cfg.rs");
}

fn main() {
    let logging_path = PathBuf::from("../config/logging.toml");
    let log_cfg_path = PathBuf::from("../tools/build_log_cfg.rs");
    let multiboot2_linker_path = PathBuf::from("linker-multiboot2.ld");
    println!("cargo:rerun-if-changed={}", logging_path.display());
    println!("cargo:rerun-if-changed={}", log_cfg_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        multiboot2_linker_path.display()
    );
    println!("cargo:rustc-check-cfg=cfg(rustos_boot_image)");

    let logging = fs::read_to_string(&logging_path).expect("failed to read shared logging config");
    shared::emit_log_cfgs(&logging);
    shared::emit_boot_trace_cfg(&logging);
}
