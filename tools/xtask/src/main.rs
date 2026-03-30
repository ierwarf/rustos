mod build;
mod cli;
mod config;
mod package_manifest;
mod qemu;
mod stage;
mod util;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}
