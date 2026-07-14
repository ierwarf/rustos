fn main() {
    // Build scripts cannot observe Rust's `cfg(test)`.  The explicit native
    // test feature is therefore the only path that omits rootd's freestanding
    // ELF contract and preserves Cargo's host test entrypoint.
    if std::env::var_os("CARGO_FEATURE_HOST_TEST").is_some() {
        return;
    }

    println!("cargo:rustc-link-arg-bin=rootd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=rootd=-no-pie");
    println!("cargo:rustc-link-arg-bin=rootd=-static");
    println!("cargo:rustc-link-arg-bin=rootd=-Wl,--image-base=0x8000400000");
    println!("cargo:rustc-link-arg-bin=rootd=-Wl,-e,_start");
}
