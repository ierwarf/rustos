use super::{
    canonical_forward_dll_name, parse_def_exports, parse_objdump_imports, reject_contract_token,
};
use fs_err as fs;
use std::path::Path;

#[test]
fn c_assembly_sources_are_closed_and_freshness_includes_every_unit() {
    let root = tempfile::tempdir().unwrap();
    let c = root.path().join("probe.c");
    let asm = root.path().join("registers.S");
    let output = root.path().join("probe.elf");
    let manifest = root.path().join("RUSTOS.package.toml");
    for path in [&c, &asm, &output, &manifest] { fs::write(path, b"input").unwrap(); }
    let sources = super::c_demo_sources(c.clone(), root.path(), &["registers.S".into()]).unwrap();
    assert_eq!(sources, vec![c.clone(), asm.clone()]);
    let epoch = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
    for (path, seconds) in [(&c, 0), (&manifest, 0), (&output, 1), (&asm, 2)] {
        std::fs::File::open(path).unwrap().set_times(std::fs::FileTimes::new()
            .set_modified(epoch + std::time::Duration::from_secs(seconds))).unwrap();
    }
    assert!(super::output_is_fresh(&output, &[c]).unwrap());
    let inputs = super::c_demo_inputs(&sources, &manifest);
    assert_eq!(inputs, vec![sources[0].clone(), asm, manifest]);
    assert!(!super::output_is_fresh(&output, &inputs).unwrap(), "assembly change must rebuild ELF");
    for alias in ["", "/outside.S", "../outside.S", "./registers.S", "probe.c"] {
        assert!(super::c_demo_sources(sources[0].clone(), root.path(), &[alias.into()]).is_err());
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn hecs_native_assembly_comparator_detects_gpr_xmm_and_mxcsr_faults() {
    // Literal includes make both external witness inputs visible to Cargo
    // and to the mutation evidence cache, not just runtime file reads.
    let source = include_str!("../../../../apps/smpqual/registers.S");
    let driver_source = include_str!("../../../../apps/smpqual/tests/registers_native.c");
    let temp = tempfile::tempdir().unwrap();
    let probe = temp.path().join("registers.S");
    let driver = temp.path().join("registers_native.c");
    fs::write(&probe, source).unwrap();
    fs::write(&driver, driver_source).unwrap();
    let output = temp.path().join("hecs-native");
    let cc = std::ffi::OsString::from("cc");
    assert_eq!(source.matches("    syscall\n").count(), 1);
    super::build_c_demo(&cc, &[driver.clone(), probe], &output,
        &["-O2", "-Wall", "-Wextra", "-Werror"]).unwrap();
    assert!(std::process::Command::new(&output).output().unwrap().status.success());
    // This challenges the SAME assembly comparator. It is deliberately not
    // a claim about the RustOS kernel or a privileged machine return.
    for (fault, code) in [("xorq %r12, %r12", 18), ("pxor %xmm15, %xmm15", 79),
        ("ldmxcsr 400(%rsp)", 96)] {
        let mutated = temp.path().join("registers-fault.S");
        fs::write(&mutated, source.replacen("    syscall\n",
            &format!("    syscall\n    {fault}\n"), 1)).unwrap();
        super::build_c_demo(&cc, &[driver.clone(), mutated], &output, &["-O2"]).unwrap();
        let failed = std::process::Command::new(&output).output().unwrap();
        assert_eq!(failed.status.code(), Some(1), "fault={fault}");
        assert!(String::from_utf8_lossy(&failed.stderr).contains(&format!("mismatch={code}")));
    }
}

#[test]
fn retired_dvm_display_path_guard_fails_closed() {
    let path = Path::new("rustos-dvm-display.c");
    assert!(reject_contract_token("active GPU path", path, "render_legacy").is_ok());
    let error = reject_contract_token("render_legacy", path, "render_legacy")
        .expect_err("retired renderer must be rejected");
    assert!(
        error
            .to_string()
            .contains("retired DVM display path returned")
    );
}

#[test]
fn parse_objdump_imports_stops_after_import_tables() {
    let text = r#"
The Import Tables (interpreted .idata section contents)
 vma:            Hint    Time      Forward  DLL       First
             Table   Stamp     Chain    Name      Thunk
 0000d000	0000d040 00000000 00000000 0000d62c 0000d1c8

	DLL Name: KERNEL32.dll
	vma:  Hint/Ord Member-Name Bound-To
	d350	  281  DeleteCriticalSection
	d368	  317  EnterCriticalSection

 0000d014	0000d0b0 00000000 00000000 0000d6c4 0000d238

	DLL Name: msvcrt.dll
	vma:  Hint/Ord Member-Name Bound-To
	d568	  953  fprintf
	d572	  955  fputc

The Function Table (interpreted .pdata section contents)
vma:			BeginAddress	 EndAddress	  UnwindData
 000000000040a000:	0000000000401000 0000000000401001 000000000040b000
"#;

    let imports = parse_objdump_imports(text);
    assert_eq!(
        imports,
        vec![
            (
                String::from("kernel32.dll"),
                String::from("DeleteCriticalSection"),
            ),
            (
                String::from("kernel32.dll"),
                String::from("EnterCriticalSection"),
            ),
            (String::from("msvcrt.dll"), String::from("fprintf")),
            (String::from("msvcrt.dll"), String::from("fputc")),
        ]
    );
}

#[test]
fn parse_objdump_imports_ignores_text_before_import_tables() {
    let text = r#"
There is an import table in .idata at 0x40d000
 000000000040a000:	0000000000401000 0000000000401001 000000000040b000
The Import Tables (interpreted .idata section contents)

	DLL Name: KERNEL32.dll
	vma:  Hint/Ord Member-Name Bound-To
	d40c	 1407  Sleep
"#;

    let imports = parse_objdump_imports(text);
    assert_eq!(
        imports,
        vec![(String::from("kernel32.dll"), String::from("Sleep"))]
    );
}

#[test]
fn parse_def_exports_recognizes_forwarders() {
    let temp_dir =
        std::env::temp_dir().join(format!("rustos-xtask-def-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let path = temp_dir.join("exports.def");
    fs::write(
        &path,
        "LIBRARY kernel32.dll\nEXPORTS\n    GetProcAddress=KERNELBASE.GetProcAddress\n    Sleep\n",
    )
    .unwrap();

    let exports = parse_def_exports(&path).unwrap();
    assert_eq!(exports.len(), 2);
    assert_eq!(exports[0].name, "GetProcAddress");
    assert_eq!(exports[0].forward_dll.as_deref(), Some("kernelbase.dll"));
    assert_eq!(exports[0].forward_symbol.as_deref(), Some("GetProcAddress"));
    assert_eq!(exports[1].name, "Sleep");
    assert_eq!(exports[1].forward_dll, None);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn canonical_forward_dll_name_adds_missing_suffix() {
    assert_eq!(canonical_forward_dll_name("KERNELBASE"), "kernelbase.dll");
    assert_eq!(
        canonical_forward_dll_name("vcruntime140.dll"),
        "vcruntime140.dll"
    );
}
