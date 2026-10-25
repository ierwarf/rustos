fn main() {
    println!("cargo:rustc-link-arg-bin=vfsd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=vfsd=-static-pie");
    println!("cargo:rustc-link-arg-bin=vfsd=-Wl,--no-dynamic-linker");
    println!("cargo:rustc-link-arg-bin=vfsd=-Wl,-e,_start");
}
