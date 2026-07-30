fn main() {
    println!("cargo:rustc-check-cfg=cfg(rustos_boot_image)");
}
