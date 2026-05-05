use std::fs;
use std::path::PathBuf;

mod shared {
    include!("../../tools/build_log_cfg.rs");
}

fn main() {
    let settings_path = PathBuf::from("../../settings.rs");
    let logging_path = PathBuf::from("../../config/logging.toml");
    let log_cfg_path = PathBuf::from("../../tools/build_log_cfg.rs");
    println!("cargo:rerun-if-changed={}", settings_path.display());
    println!("cargo:rerun-if-changed={}", logging_path.display());
    println!("cargo:rerun-if-changed={}", log_cfg_path.display());
    println!("cargo:rustc-check-cfg=cfg(rustos_boot_image)");
    println!("cargo:rustc-check-cfg=cfg(rustos_kernel_physical_kaslr_enabled)");

    let settings = fs::read_to_string(&settings_path).expect("failed to read shared settings");
    let logging = fs::read_to_string(&logging_path).expect("failed to read shared logging config");
    shared::emit_log_cfgs(&logging);
    shared::emit_boot_trace_cfg(&logging);
    if shared::parse_bool(&settings, "KERNEL_PHYSICAL_KASLR_ENABLED") {
        println!("cargo:rustc-cfg=rustos_kernel_physical_kaslr_enabled");
    }
}
