use std::path::PathBuf;

mod shared {
    include!("../tools/build_log_cfg.rs");
}

fn main() {
    let logging_path = PathBuf::from("../config/rustos.toml");
    let log_cfg_path = PathBuf::from("../tools/build_log_cfg.rs");
    let multiboot2_linker_path = PathBuf::from("linker-multiboot2.ld");
    shared::emit_project_config_rerun(&logging_path);
    println!("cargo:rerun-if-changed={}", log_cfg_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        multiboot2_linker_path.display()
    );
    println!("cargo:rustc-check-cfg=cfg(rustos_boot_image)");

    let logging =
        shared::read_project_config(&logging_path).expect("failed to read shared RustOS config");
    shared::emit_log_cfgs(&logging);
    shared::emit_boot_trace_cfg(&logging);
}
