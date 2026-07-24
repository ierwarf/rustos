mod build;
mod cli;
mod config;
mod dev;
mod kvm;
mod layering;
mod package_manifest;
mod stage;
mod storage_epoch;
mod testinfra;
mod util;

type Result<T> = anyhow::Result<T>;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}
