use std::fs;
use std::path::PathBuf;

mod shared {
    include!("../../tools/build_log_cfg.rs");
}

fn main() {
    let logging_path = PathBuf::from("../../config/logging.toml");
    let log_cfg_path = PathBuf::from("../../tools/build_log_cfg.rs");
    println!("cargo:rerun-if-changed={}", logging_path.display());
    println!("cargo:rerun-if-changed={}", log_cfg_path.display());
    println!("cargo:rustc-check-cfg=cfg(rustos_building_kernel_compat)");
    println!("cargo:rustc-cfg=rustos_building_kernel_compat");

    let logging = fs::read_to_string(&logging_path).expect("failed to read shared logging config");
    shared::emit_log_cfgs(&logging);
    shared::emit_boot_trace_cfg(&logging);
}
