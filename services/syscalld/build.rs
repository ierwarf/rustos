fn main() {
    println!("cargo:rustc-link-arg-bin=syscalld=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=syscalld=-no-pie");
    println!("cargo:rustc-link-arg-bin=syscalld=-static");
    println!("cargo:rustc-link-arg-bin=syscalld=-Wl,--image-base=0x8000400000");
    println!("cargo:rustc-link-arg-bin=syscalld=-Wl,-e,_start");
}
