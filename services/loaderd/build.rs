fn main() {
    println!("cargo:rustc-link-arg-bin=loaderd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=loaderd=-no-pie");
    println!("cargo:rustc-link-arg-bin=loaderd=-static");
    println!("cargo:rustc-link-arg-bin=loaderd=-Wl,--image-base=0x8000400000");
    println!("cargo:rustc-link-arg-bin=loaderd=-Wl,-e,_start");
}
