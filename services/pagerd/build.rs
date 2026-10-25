fn main() {
    println!("cargo:rustc-link-arg-bin=pagerd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=pagerd=-static-pie");
    println!("cargo:rustc-link-arg-bin=pagerd=-Wl,--no-dynamic-linker");
    println!("cargo:rustc-link-arg-bin=pagerd=-Wl,-e,_start");
}
