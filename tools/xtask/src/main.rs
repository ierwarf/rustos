mod build;
mod cli;
mod config;
mod layering;
mod package_manifest;
mod ring3_inventory;
mod stage;
mod testinfra;
mod util;
mod xen;

type Result<T> = anyhow::Result<T>;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}
