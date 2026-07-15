mod build;
mod cli;
mod config;
mod kvm;
mod layering;
mod package_manifest;
mod stage;
mod testinfra;
mod util;

type Result<T> = anyhow::Result<T>;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}
