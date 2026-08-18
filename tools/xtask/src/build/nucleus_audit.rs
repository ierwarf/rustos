//! Post-link audits of the linked nucleus artifact.
//!
//! Two checks read the artifact after it links and before it is signed:
//! Multiboot2 compliance, and the FPU/SIMD custody the kernel entry paths
//! claim. Both fail the build rather than warn.
//!
//! # FPU/SIMD custody
//!
//! - **Owner:** this module owns the shipped-image side of the FPU custody
//!   contract. The register-level side is owned by the syscall entry stub
//!   (`kernel/compat/src/user/syscall/mod.rs`) and the interrupt stubs
//!   (`kernel/hal/src/interrupt_stubs.rs`).
//! - **Boundary:** it reads a linked ELF through `objdump -d` and nothing else.
//!   No source path, crate graph, or build configuration crosses this boundary.
//! - **Lifecycle:** it runs once per nucleus link, before the artifact is
//!   signed, so an unsigned image is never audited and a signed one is never
//!   unaudited.
//! - **Failure:** any violation fails the build. There is no warning mode: the
//!   entry paths do not save the state a violation would disturb, so a
//!   violation is a live register leak into userspace, not a style defect.
//! - **Forbidden:** no allowlist entry without a named bracket that covers it.
//!
//! # The contract being audited
//!
//! Both kernel entry paths save and restore `xmm0`-`xmm15`, and nothing else.
//! They do **not** save x87, they do **not** save `MXCSR`, and a legacy SSE
//! `movdqu` restore leaves `ymm` bits 255:128 exactly as the kernel left them.
//! So the kernel gets to keep the user's FPU state only while three properties
//! hold of the linked image:
//!
//! 1. no x87 instruction executes in the kernel;
//! 2. no floating-point *arithmetic* instruction executes in the kernel, since
//!    those are what write `MXCSR`'s status flags; and
//! 3. every VEX/EVEX-encoded instruction sits inside a function that runs under
//!    an explicit `kernel_hal::arch::simd::wide_simd_section()` bracket.
//!
//! Linux gets the first two from `-msoft-float` on ordinary kernel code, with
//! hard float confined to `kernel_fpu_begin()` sections. Rust cannot express
//! that split on x86-64: measured, `-C target-feature=+soft-float` emits no
//! XMM/YMM even inside a `#[target_feature(enable = "avx")]` function, and the
//! kernel target's baseline includes SSE2 (rust-lang/rust#136540, #133611,
//! #116344). This audit is how the property is obtained instead — verified on
//! the artifact rather than promised by a compiler flag.
//!
//! Data movement and bitwise operations are not arithmetic. `movaps`, `movups`,
//! and `xorps` are how the compiler moves 16-byte values around; none of them
//! touches `MXCSR`. Only the operations that can raise an SSE exception flag
//! matter here.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail};

use crate::Result;
use crate::config::Config;

/// Symbols permitted to contain VEX/EVEX-encoded instructions, matched as
/// substrings of the v0-mangled name, each with the bracket that covers it.
///
/// An entry here is a claim that every kernel path reaching the symbol runs
/// under a wide-SIMD bracket. Adding one without that bracket reintroduces the
/// leak this audit exists to catch.
const WIDE_SIMD_ALLOWLIST: &[(&str, &str)] = &[
    (
        "kernel_hal4arch4simd",
        "the custody module itself: `copy_ymm` and the bracket's own \
         save/restore are where wide SIMD is allowed to appear",
    ),
    (
        "curve25519_dalek",
        "ed25519 epoch-signature verification, bracketed at \
         `kernel/io-manager/src/io/dvm_block.rs::verify_epoch_signature_with_key`",
    ),
    (
        "4sha2",
        "SHA-512 inside that same ed25519 verification, under the same bracket; \
         the SHA-256 boot-volume digest compiles to non-VEX SHA-NI",
    ),
];

/// Instructions that begin with `f` but are FPU *custody*, not FPU use.
///
/// These are how `kernel_hal::arch::simd` saves and restores the x87 area on
/// behalf of a user task, which is the one legitimate reason for x87 state to
/// be named in kernel code.
const X87_CUSTODY_MNEMONICS: &[&str] = &["fxsave", "fxsave64", "fxrstor", "fxrstor64"];

/// Mnemonics that end in an SSE/AVX floating-point suffix but perform no
/// arithmetic, so they cannot write an `MXCSR` status flag.
///
/// Everything else carrying one of those suffixes is treated as arithmetic.
/// That over-approximates in the safe direction: a false positive fails a build
/// and gets a human's attention, while a false negative is a silent leak.
const NON_ARITHMETIC_FLOAT_MNEMONICS: &[&str] = &[
    // Data movement.
    "movaps",
    "movapd",
    "movups",
    "movupd",
    "movss",
    "movsd",
    "movlps",
    "movlpd",
    "movhps",
    "movhpd",
    "movntps",
    "movntpd",
    "movmskps",
    "movmskpd",
    "extractps",
    "insertps",
    // Bitwise.
    "andps",
    "andpd",
    "andnps",
    "andnpd",
    "orps",
    "orpd",
    "xorps",
    "xorpd",
    // Lane selection.
    "shufps",
    "shufpd",
    "unpcklps",
    "unpcklpd",
    "unpckhps",
    "unpckhpd",
    "blendps",
    "blendpd",
    "blendvps",
    "blendvpd",
    "permilps",
    "permilpd",
    "perm2f128",
    "maskmovps",
    "maskmovpd",
    "testps",
    "testpd",
    "broadcastss",
    "broadcastsd",
];

/// Instructions whose mnemonic begins with `v` without being VEX/EVEX-encoded.
const NON_VEX_V_MNEMONICS: &[&str] = &[
    "verr", "verw", "vmcall", "vmclear", "vmfunc", "vmlaunch", "vmload", "vmmcall", "vmptrld",
    "vmptrst", "vmread", "vmresume", "vmrun", "vmsave", "vmwrite", "vmxoff", "vmxon",
];

const FLOAT_SUFFIXES: &[&str] = &["ss", "sd", "ps", "pd"];

/// One offending instruction, named by the symbol that contains it.
#[derive(Debug, PartialEq, Eq)]
struct Site {
    symbol: String,
    mnemonic: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SimdCustodyReport {
    x87: Vec<Site>,
    float_arithmetic: Vec<Site>,
    /// Symbols outside `WIDE_SIMD_ALLOWLIST` that contain VEX/EVEX
    /// instructions, and how many each contains.
    unbracketed_wide_simd: BTreeMap<String, usize>,
    /// Instructions examined, so a parse that silently matched nothing cannot
    /// pass as a clean audit.
    instructions: usize,
}

/// Fails unless the nucleus artifact is a Multiboot2 image GRUB will load.
pub(super) fn check_nucleus_multiboot2(config: &Config) -> Result<()> {
    let artifact = config.artifact_nucleus_elf_path();
    let status = Command::new(&config.grub_file)
        .arg("--is-x86-multiboot2")
        .arg(&artifact)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "nucleus artifact is not Multiboot2-compliant: {}",
            artifact.display()
        ))
    }
}

/// Runs the Multiboot2 check only for a build that already produced an artifact.
pub(super) fn check_nucleus_multiboot2_if_present(config: &Config) -> Result<()> {
    if config.artifact_nucleus_elf_path().is_file() {
        check_nucleus_multiboot2(config)?;
    }
    Ok(())
}

/// Audits the linked nucleus image against the entry paths' custody contract.
pub(super) fn audit_simd_custody(config: &Config) -> Result<()> {
    let artifact = config.artifact_nucleus_elf_path();
    let output = Command::new(&config.objdump)
        .arg("-d")
        .arg("--no-show-raw-insn")
        .arg(&artifact)
        .output()?;
    if !output.status.success() {
        bail!("objdump failed for {}", artifact.display());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    audit_disassembly(text.as_ref()).into_result(&artifact)
}

/// Classifies every instruction in an `objdump -d --no-show-raw-insn` listing.
fn audit_disassembly(listing: &str) -> SimdCustodyReport {
    let mut report = SimdCustodyReport::default();
    let mut symbol = String::from("<no symbol>");

    for line in listing.lines() {
        if let Some(name) = symbol_header(line) {
            symbol = name.to_string();
            continue;
        }
        let Some(mnemonic) = instruction_mnemonic(line) else {
            continue;
        };
        report.instructions += 1;

        if is_x87(mnemonic) {
            report.x87.push(Site::new(&symbol, mnemonic));
        }
        if is_float_arithmetic(mnemonic) {
            report.float_arithmetic.push(Site::new(&symbol, mnemonic));
        }
        if is_vex_encoded(mnemonic) && !wide_simd_allowed(&symbol) {
            *report
                .unbracketed_wide_simd
                .entry(symbol.clone())
                .or_default() += 1;
        }
    }

    report
}

/// The symbol name in a `ffffffff80100000 <name>:` header line.
fn symbol_header(line: &str) -> Option<&str> {
    let name = line.split_once(" <")?.1;
    name.strip_suffix(">:")
}

/// The mnemonic in an `  ffffffff80100000:\tmov    %rax,%rbx` instruction line.
fn instruction_mnemonic(line: &str) -> Option<&str> {
    let (address, rest) = line.trim_start().split_once(':')?;
    if address.is_empty() || !address.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mnemonic = rest.split_whitespace().next()?;
    // objdump prints `(bad)` for bytes it cannot decode, and prefixes such as
    // `lock` or `rep` as separate leading tokens.
    if mnemonic.starts_with('(') {
        return None;
    }
    match mnemonic {
        "lock" | "rep" | "repz" | "repnz" | "data16" | "rex" | "rex.W" | "cs" | "ds" | "es"
        | "fs" | "gs" | "ss" | "notrack" | "bnd" => rest.split_whitespace().nth(1),
        _ => Some(mnemonic),
    }
}

fn is_x87(mnemonic: &str) -> bool {
    // Every x87 mnemonic begins with `f`; the only kernel instructions that do
    // so without being x87 use are the custody pair.
    mnemonic.starts_with('f') && !X87_CUSTODY_MNEMONICS.contains(&mnemonic)
}

fn is_float_arithmetic(mnemonic: &str) -> bool {
    let base = mnemonic.strip_prefix('v').unwrap_or(mnemonic);
    FLOAT_SUFFIXES
        .iter()
        .any(|suffix| base.ends_with(suffix) && base.len() > suffix.len())
        && !NON_ARITHMETIC_FLOAT_MNEMONICS.contains(&base)
}

fn is_vex_encoded(mnemonic: &str) -> bool {
    mnemonic.starts_with('v') && !NON_VEX_V_MNEMONICS.contains(&mnemonic)
}

fn wide_simd_allowed(symbol: &str) -> bool {
    WIDE_SIMD_ALLOWLIST
        .iter()
        .any(|(pattern, _)| symbol.contains(pattern))
}

impl Site {
    fn new(symbol: &str, mnemonic: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
            mnemonic: mnemonic.to_string(),
        }
    }
}

impl SimdCustodyReport {
    fn into_result(self, artifact: &Path) -> Result<()> {
        if self.instructions == 0 {
            bail!(
                "SIMD custody audit disassembled no instructions from {} -- \
                 a clean report here would be a parse failure, not a clean image",
                artifact.display()
            );
        }

        if !self.x87.is_empty() {
            bail!(
                "SIMD custody: {} x87 instruction(s) in the kernel image {}: {}. \
                 Neither entry path saves x87 state, so this overwrites the \
                 caller's FPU registers. Remove the x87 work, or give both the \
                 syscall stub and the interrupt stubs explicit x87 custody.",
                self.x87.len(),
                artifact.display(),
                describe(&self.x87),
            );
        }

        if !self.float_arithmetic.is_empty() {
            bail!(
                "SIMD custody: {} floating-point arithmetic instruction(s) in the \
                 kernel image {}: {}. Those write MXCSR's status flags, which \
                 neither entry path saves, so userspace reads kernel rounding and \
                 exception state back out of STMXCSR. Remove the floating-point \
                 work, or add an `stmxcsr`/`ldmxcsr` pair to both the syscall stub \
                 and the interrupt stubs.",
                self.float_arithmetic.len(),
                artifact.display(),
                describe(&self.float_arithmetic),
            );
        }

        if !self.unbracketed_wide_simd.is_empty() {
            let symbols = self
                .unbracketed_wide_simd
                .iter()
                .map(|(symbol, count)| format!("{symbol} ({count})"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "SIMD custody: VEX/EVEX instructions in {} unbracketed symbol(s) of \
                 the kernel image {}: {}. A VEX instruction rewrites ymm bits \
                 255:128 -- even a 128-bit one, which zeroes them -- and the entry \
                 paths restore only the low 128 bits. Wrap the caller in \
                 `kernel_hal::arch::simd::wide_simd_section()` and add the symbol \
                 to WIDE_SIMD_ALLOWLIST with the bracket that covers it.",
                self.unbracketed_wide_simd.len(),
                artifact.display(),
                symbols,
            );
        }

        Ok(())
    }
}

fn describe(sites: &[Site]) -> String {
    const SHOWN: usize = 8;
    let mut described = sites
        .iter()
        .take(SHOWN)
        .map(|site| format!("{} in {}", site.mnemonic, site.symbol))
        .collect::<Vec<_>>();
    if sites.len() > SHOWN {
        described.push(format!("and {} more", sites.len() - SHOWN));
    }
    described.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(body: &str) -> String {
        format!("\nbuild/image/nucleus.elf:     file format elf64-x86-64\n\n{body}")
    }

    #[test]
    fn ordinary_integer_and_data_movement_code_is_clean() {
        let report = audit_disassembly(&listing(
            "ffffffff80100000 <_RNvCs1_6kernel4main>:\n\
             ffffffff80100000:\tpush   %rbp\n\
             ffffffff80100001:\tmovaps %xmm0,(%rdi)\n\
             ffffffff80100005:\tmovups (%rsi),%xmm1\n\
             ffffffff80100009:\txorps  %xmm2,%xmm2\n\
             ffffffff8010000d:\tpxor   %xmm3,%xmm3\n\
             ffffffff80100011:\tret\n",
        ));
        assert_eq!(report.x87, Vec::new());
        assert_eq!(report.float_arithmetic, Vec::new());
        assert!(report.unbracketed_wide_simd.is_empty());
        assert_eq!(report.instructions, 6);
    }

    #[test]
    fn a_parse_that_matched_nothing_is_a_failure_not_a_clean_image() {
        // The whole audit is a set of absences. If the listing format ever
        // changes under it, every absence holds vacuously and the build would
        // pass with no evidence at all.
        let report = audit_disassembly("objdump: unrecognized option\n");
        assert_eq!(report.instructions, 0);
        assert!(report.into_result(Path::new("nucleus.elf")).is_err());
    }

    #[test]
    fn float_arithmetic_is_rejected_because_it_writes_mxcsr() {
        let report = audit_disassembly(&listing(
            "ffffffff80100000 <_RNvCs1_6kernel5scale>:\n\
             ffffffff80100000:\tmulsd  %xmm1,%xmm0\n\
             ffffffff80100004:\tcvtsi2sd %rax,%xmm1\n\
             ffffffff80100009:\tsqrtpd %xmm0,%xmm2\n\
             ffffffff8010000d:\tret\n",
        ));
        assert_eq!(
            report
                .float_arithmetic
                .iter()
                .map(|site| site.mnemonic.as_str())
                .collect::<Vec<_>>(),
            ["mulsd", "cvtsi2sd", "sqrtpd"],
        );
        let message = report
            .into_result(Path::new("nucleus.elf"))
            .expect_err("float arithmetic must fail the audit")
            .to_string();
        assert!(message.contains("MXCSR"), "{message}");
    }

    #[test]
    fn x87_is_rejected_but_the_custody_pair_that_saves_it_is_not() {
        let clean = audit_disassembly(&listing(
            "ffffffff80100000 <_RNvNtCs2_10kernel_hal4simd10save_state>:\n\
             ffffffff80100000:\tfxsave64 (%rdi)\n\
             ffffffff80100004:\tfxrstor64 (%rdi)\n\
             ffffffff80100008:\tret\n",
        ));
        assert_eq!(clean.x87, Vec::new());

        let dirty = audit_disassembly(&listing(
            "ffffffff80100000 <_RNvCs1_6kernel5angle>:\n\
             ffffffff80100000:\tfldl   (%rdi)\n\
             ffffffff80100003:\tfmul   %st(1),%st\n\
             ffffffff80100005:\tfstpl  (%rsi)\n",
        ));
        assert_eq!(
            dirty
                .x87
                .iter()
                .map(|site| site.mnemonic.as_str())
                .collect::<Vec<_>>(),
            ["fldl", "fmul", "fstpl"],
        );
    }

    #[test]
    fn vex_outside_the_allowlist_is_rejected_and_inside_it_is_not() {
        let allowed = audit_disassembly(&listing(
            "ffffffff80100000 <_RNvNtNtCs2_10kernel_hal4arch4simd8copy_ymm>:\n\
             ffffffff80100000:\tvmovdqu (%rdi),%ymm0\n\
             ffffffff80100004:\tvzeroupper\n",
        ));
        assert!(allowed.unbracketed_wide_simd.is_empty());

        let rejected = audit_disassembly(&listing(
            "ffffffff80100000 <_RNvNtCs3_9kernel_ps6hasher>:\n\
             ffffffff80100000:\tvpaddq %ymm1,%ymm2,%ymm3\n\
             ffffffff80100004:\tvzeroupper\n",
        ));
        assert_eq!(
            rejected.unbracketed_wide_simd,
            BTreeMap::from([("_RNvNtCs3_9kernel_ps6hasher".to_string(), 2)]),
        );
    }

    #[test]
    fn vmx_and_segment_verification_are_not_vex_instructions() {
        let report = audit_disassembly(&listing(
            "ffffffff80100000 <_RNvNtCs3_9kernel_ps4vmxon>:\n\
             ffffffff80100000:\tvmxon  (%rdi)\n\
             ffffffff80100004:\tvmread %rax,%rbx\n\
             ffffffff80100008:\tverw   %ax\n",
        ));
        assert!(report.unbracketed_wide_simd.is_empty());
        assert_eq!(report.instructions, 3);
    }

    #[test]
    fn instruction_prefixes_do_not_hide_the_mnemonic() {
        assert_eq!(
            instruction_mnemonic("  ffffffff80100000:\tlock cmpxchg %rbx,(%rdi)"),
            Some("cmpxchg"),
        );
        assert_eq!(
            instruction_mnemonic("  ffffffff80100000:\trep stos %al,%es:(%rdi)"),
            Some("stos"),
        );
        assert_eq!(instruction_mnemonic("  ffffffff80100000:\t(bad)"), None);
        assert_eq!(instruction_mnemonic("Disassembly of section .text:"), None);
    }

    #[test]
    fn every_allowlist_entry_states_the_bracket_that_covers_it() {
        for (pattern, reason) in WIDE_SIMD_ALLOWLIST {
            assert!(!pattern.is_empty(), "empty allowlist pattern");
            assert!(
                reason.contains("bracket") || reason.contains("custody"),
                "allowlist entry {pattern} does not name its bracket: {reason}",
            );
        }
    }
}
