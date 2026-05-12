fn main() {
    println!("cargo:rustc-link-arg-bin=rootd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=rootd=-no-pie");
    println!("cargo:rustc-link-arg-bin=rootd=-static");
    println!("cargo:rustc-link-arg-bin=rootd=-Wl,--image-base=0x8000400000");
    println!("cargo:rustc-link-arg-bin=rootd=-Wl,-e,_start");
}
