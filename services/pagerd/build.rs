fn main() {
    println!("cargo:rustc-link-arg-bin=pagerd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=pagerd=-no-pie");
    println!("cargo:rustc-link-arg-bin=pagerd=-static");
    println!("cargo:rustc-link-arg-bin=pagerd=-Wl,--image-base=0x8000c00000");
    println!("cargo:rustc-link-arg-bin=pagerd=-Wl,-e,_start");
}
