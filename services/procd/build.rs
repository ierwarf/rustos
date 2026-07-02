fn main() {
    println!("cargo:rustc-link-arg-bin=procd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=procd=-no-pie");
    println!("cargo:rustc-link-arg-bin=procd=-static");
    println!("cargo:rustc-link-arg-bin=procd=-Wl,--image-base=0x8000400000");
    println!("cargo:rustc-link-arg-bin=procd=-Wl,-e,_start");
}
