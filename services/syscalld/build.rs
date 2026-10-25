fn main() {
    println!("cargo:rustc-link-arg-bin=syscalld=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=syscalld=-static-pie");
    println!("cargo:rustc-link-arg-bin=syscalld=-Wl,--no-dynamic-linker");
    println!("cargo:rustc-link-arg-bin=syscalld=-Wl,-e,_start");
}
