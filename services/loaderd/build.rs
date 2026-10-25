fn main() {
    println!("cargo:rustc-link-arg-bin=loaderd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=loaderd=-static-pie");
    println!("cargo:rustc-link-arg-bin=loaderd=-Wl,--no-dynamic-linker");
    println!("cargo:rustc-link-arg-bin=loaderd=-Wl,-e,_start");
}
