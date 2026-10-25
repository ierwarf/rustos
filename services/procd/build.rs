fn main() {
    println!("cargo:rustc-link-arg-bin=procd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=procd=-static-pie");
    println!("cargo:rustc-link-arg-bin=procd=-Wl,--no-dynamic-linker");
    println!("cargo:rustc-link-arg-bin=procd=-Wl,-e,_start");
}
