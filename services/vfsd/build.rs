fn main() {
    println!("cargo:rustc-link-arg-bin=vfsd=-nostartfiles");
    println!("cargo:rustc-link-arg-bin=vfsd=-no-pie");
    println!("cargo:rustc-link-arg-bin=vfsd=-static");
    println!("cargo:rustc-link-arg-bin=vfsd=-Wl,--image-base=0x8000400000");
    println!("cargo:rustc-link-arg-bin=vfsd=-Wl,-e,_start");
}
