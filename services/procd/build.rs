fn main() {
    // Build scripts cannot observe Rust's `cfg(test)`. The explicit native
    // test feature is therefore the only path that omits procd's freestanding
    // ELF contract and preserves Cargo's host test entrypoint.
    if std::env::var_os("CARGO_FEATURE_HOST_TEST").is_some() {
        return;
    }

    println!("cargo:rustc-link-arg-bin=procd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=procd=-static-pie");
    println!("cargo:rustc-link-arg-bin=procd=-Wl,--no-dynamic-linker");
    println!("cargo:rustc-link-arg-bin=procd=-Wl,-e,_start");
}
