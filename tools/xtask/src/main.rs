mod bench;
mod build;
mod cli;
mod config;
mod dev;
mod formal_contracts;
mod kvm;
mod layering;
mod package_manifest;
mod soak;
mod stage;
mod storage_epoch;
mod testinfra;
mod util;

type Result<T> = anyhow::Result<T>;

fn main() {
    if let Err(err) = cli::run() {
        // `{err}` prints only the outermost context, which hides the cause a
        // failing lane was written to report. Every layer added a context
        // string precisely so the reader gets the chain, so print it.
        eprintln!("xtask: {err:#}");
        std::process::exit(1);
    }
}
