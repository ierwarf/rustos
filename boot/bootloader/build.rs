use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=src/settings.rs");
    println!("cargo:rustc-check-cfg=cfg(rustos_debug_print_enabled)");

    let settings =
        fs::read_to_string("src/settings.rs").expect("failed to read bootloader settings");
    if parse_bool(&settings, "DEBUG_PRINT_ENABLED") {
        println!("cargo:rustc-cfg=rustos_debug_print_enabled");
    }
}

fn parse_bool(source: &str, name: &str) -> bool {
    let prefix = format!("pub const {name}: bool =");
    for line in source.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix(&prefix) {
            return match value.trim().trim_end_matches(';') {
                "true" => true,
                "false" => false,
                other => panic!("invalid bool for {name}: {other}"),
            };
        }
    }

    panic!("missing bool constant: {name}");
}
