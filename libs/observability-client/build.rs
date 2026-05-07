use std::fs;
use std::path::PathBuf;

mod shared {
    include!("../../tools/build_log_cfg.rs");
}

fn main() {
    let logging_path = PathBuf::from("../../config/rustos.toml");
    let log_cfg_path = PathBuf::from("../../tools/build_log_cfg.rs");
    shared::emit_project_config_rerun(&logging_path);
    println!("cargo:rerun-if-changed={}", log_cfg_path.display());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("missing OUT_DIR"));
    let generated_helpers_path = out_dir.join("logging_helpers.rs");

    let logging =
        shared::read_project_config(&logging_path).expect("failed to read shared RustOS config");
    shared::emit_log_cfgs(&logging);
    fs::write(
        &generated_helpers_path,
        shared::generate_user_logging_helpers_rs(),
    )
    .expect("failed to write generated logging helpers");
}
