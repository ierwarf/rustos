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
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("missing OUT_DIR"));
    let generated_path = out_dir.join("logging_build.rs");
    let generated_macros_path = out_dir.join("logging_macros.rs");

    let logging = fs::read_to_string(&logging_path).expect("failed to read shared logging config");
    let logging_config = shared::parse_logging_toml(&logging);
    shared::emit_log_cfgs(&logging);
    shared::emit_boot_trace_cfg(&logging);

    fs::write(
        &generated_path,
        shared::generate_kernel_logging_build_rs(&logging_config),
    )
    .expect("failed to write generated logging helpers");
    fs::write(
        &generated_macros_path,
        shared::generate_kernel_logging_macros_rs(),
    )
    .expect("failed to write generated logging macros");
}
