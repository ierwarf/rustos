use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};

mod shared {
    include!("../../tools/build_log_cfg.rs");
}

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("failed to resolve repo root from CARGO_MANIFEST_DIR")?;
    let logging_path = repo_root.join("config/rustos.toml");
    let log_cfg_path = repo_root.join("tools/build_log_cfg.rs");

    shared::emit_project_config_rerun(&logging_path);
    println!("cargo:rerun-if-changed={}", log_cfg_path.display());
    let logging = shared::read_project_config(&logging_path)?;
    shared::emit_log_cfgs(&logging);
    Ok(())
}
