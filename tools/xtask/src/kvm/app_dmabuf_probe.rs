// Evidence for `kvm-smoke --app-dmabuf-probe`: one real application DMA-BUF
// travels allocation (client -> devmgrd -> kernel broker -> Linux DVM export),
// mmap/fill, Wayland import by uiserver, snapshot, release, and finally
// revoke + DVM acknowledgement + scrub, while a forged descriptor is refused by
// the real compositor. Owner contract: `docs/ai/abi/app-dmabuf.md`.
//
// The probe is armed only by a private contract file on the per-run boot disk;
// the production image has no such file and `apps/wayclick` stays on `wl_shm`.

const PRIVATE_APP_DMABUF_PROBE_CONTRACT_PATH: &str = "system/registry/system/appdmabuf-probe-v1.env";
/// Must equal `APP_DMABUF_PROBE_CONTRACT_BODY` in `apps/wayclick`.
const APP_DMABUF_PROBE_CONTRACT_BODY: &[u8] = b"contract=rustos-appdmabuf-probe-v1\n";
/// Any line containing this ends the boot at once (guest and services).
const APP_DMABUF_PROBE_FAIL_MARKER: &str = "app-dmabuf-probe: FAIL";

// The kernel half of the chain is reported as debug milestones (the only kernel
// channel that reaches debugcon in the product build), `name=... arg0= arg1=`:
//   export/revoke-published  arg0=seq   arg1=(offset pages << 32) | length pages
//   export/revoke-ack        arg0=seq   arg1=provider status (0 = success)
//   allocation-granted       arg0=fd    arg1=length bytes
//   range-reclaimed          arg0=0     arg1=(offset pages << 32) | length pages
//                            (emitted only after the scrub succeeded)

/// Splits the packed range of a published/reclaimed milestone into bytes.
fn app_dmabuf_packed_range(line: &str) -> Option<(u64, u64)> {
    let packed = app_dmabuf_field(line, "arg1=")?;
    Some(((packed >> 32) * 4096, (packed & 0xffff_ffff) * 4096))
}

/// The allocation chain in the order the system must perform it. Each entry is
/// a substring of exactly one log line; `validate_app_dmabuf_probe_log` checks
/// that the first occurrence of each follows the previous one.
const APP_DMABUF_PROBE_ALLOCATION_CHAIN: [&str; 7] = [
    "name=app-dmabuf-export-published ",
    "name=app-dmabuf-export-ack ",
    "name=app-dmabuf-allocation-granted ",
    "wayclick: app-dmabuf allocated index=",
    "uiserver: app-dmabuf import accepted width=",
    "uiserver: app-dmabuf snapshot ok bytes=",
    "uiserver: app-dmabuf buffer released",
];
/// After the client is gone: revoke, DVM acknowledgement, scrubbed reuse.
const APP_DMABUF_PROBE_RETIREMENT_CHAIN: [&str; 3] = [
    "name=app-dmabuf-revoke-published ",
    "name=app-dmabuf-revoke-ack ",
    "name=app-dmabuf-range-reclaimed ",
];
/// Negative proof: a forged descriptor is refused, never imported.
const APP_DMABUF_PROBE_NEGATIVE: [&str; 2] = [
    "uiserver: app-dmabuf import refused reason=NotAppBuffer",
    "wayclick: app-dmabuf probe forged-fd refused",
];
const APP_DMABUF_PROBE_PASSED_MARKER: &str = "wayclick: app-dmabuf probe passed";

fn app_dmabuf_probe_markers() -> Vec<&'static str> {
    let mut markers = Vec::new();
    markers.extend(APP_DMABUF_PROBE_ALLOCATION_CHAIN);
    markers.extend(APP_DMABUF_PROBE_NEGATIVE);
    markers.push(APP_DMABUF_PROBE_PASSED_MARKER);
    markers.extend(APP_DMABUF_PROBE_RETIREMENT_CHAIN);
    markers
}

/// The probe's own `FAIL` record, trimmed of the debugcon framing, if present.
fn app_dmabuf_probe_failure(log: &str) -> Option<String> {
    let line = log.lines().find(|line| line.contains(APP_DMABUF_PROBE_FAIL_MARKER))?;
    let at = line.find(APP_DMABUF_PROBE_FAIL_MARKER)?;
    Some(line[at..].trim().trim_end_matches("\\n").to_owned())
}

fn app_dmabuf_field(line: &str, key: &str) -> Option<u64> {
    let rest = line.split(key).nth(1)?;
    let token = rest.split(|c: char| c.is_whitespace() || c == ',').next()?;
    match token.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => token.parse().ok(),
    }
}

/// Byte range the kernel exported to the DVM, from the validated log.
#[derive(Debug, PartialEq, Eq)]
struct AppDmabufProbeEvidence {
    offset: u64,
    length: u64,
}

fn validate_app_dmabuf_probe_log(log: &str) -> Result<AppDmabufProbeEvidence> {
    if let Some(failure) = app_dmabuf_probe_failure(log) {
        bail!("app DMA-BUF probe reported a failure: {failure}");
    }
    let lines: Vec<&str> = log.lines().collect();
    let first = |marker: &str, after: usize| {
        lines
            .iter()
            .enumerate()
            .skip(after)
            .find(|(_, line)| line.contains(marker))
            .map(|(index, _)| index)
    };
    let mut cursor = 0;
    for marker in APP_DMABUF_PROBE_ALLOCATION_CHAIN {
        let Some(index) = first(marker, cursor) else {
            bail!("app DMA-BUF probe never logged `{marker}` in order");
        };
        cursor = index;
    }
    let published = lines[first(APP_DMABUF_PROBE_ALLOCATION_CHAIN[0], 0).unwrap()];
    let ack = lines[first(APP_DMABUF_PROBE_ALLOCATION_CHAIN[1], 0).unwrap()];
    if app_dmabuf_field(ack, "arg1=") != Some(0) {
        bail!("DVM export acknowledgement was not a success: {ack}");
    }
    if app_dmabuf_field(published, "arg0=") != app_dmabuf_field(ack, "arg0=") {
        bail!("DVM export acknowledged a different sequence than was published: {published} / {ack}");
    }
    let Some((offset, length)) = app_dmabuf_packed_range(published) else {
        bail!("export record has no range: {published}");
    };
    let aperture_end = offset.checked_add(length);
    if length == 0 || length % 4096 != 0 || offset % 4096 != 0 || aperture_end.is_none_or(|end| end > DVM_APP_BUFFER_REGION_BYTES) {
        bail!("export range is not a page-aligned range inside the aperture: {published}");
    }
    // The retirement must be for that same range and follow the import.
    let mut cursor = first("uiserver: app-dmabuf import accepted width=", 0).unwrap_or(0);
    for marker in APP_DMABUF_PROBE_RETIREMENT_CHAIN {
        let Some(index) = first(marker, cursor) else {
            bail!("app DMA-BUF probe never logged `{marker}` after the import");
        };
        cursor = index;
    }
    let revoke_ack = lines[first(APP_DMABUF_PROBE_RETIREMENT_CHAIN[1], 0).unwrap()];
    if app_dmabuf_field(revoke_ack, "arg1=") != Some(0) {
        bail!("DVM revoke acknowledgement was not a success: {revoke_ack}");
    }
    let reclaimed = lines
        .iter()
        .filter(|line| line.contains(APP_DMABUF_PROBE_RETIREMENT_CHAIN[2]))
        .any(|line| {
            app_dmabuf_packed_range(line) == Some((offset, length))
        });
    if !reclaimed {
        bail!("the exported range {offset:#x}+{length:#x} was never scrubbed and reclaimed");
    }
    for marker in APP_DMABUF_PROBE_NEGATIVE {
        if first(marker, 0).is_none() {
            bail!("app DMA-BUF probe never logged `{marker}`");
        }
    }
    if first(APP_DMABUF_PROBE_PASSED_MARKER, 0).is_none() {
        bail!("wayclick never logged `{APP_DMABUF_PROBE_PASSED_MARKER}`");
    }
    Ok(AppDmabufProbeEvidence { offset, length })
}

#[cfg(test)]
mod app_dmabuf_probe_tests {
    use super::*;

    fn ms(name: &str, arg0: u64, arg1: u64) -> String {
        format!("seq=9 lvl=info cat=compat msg=\"milestone-begin v=1 cat=compat name={name} arg0={arg0:#x} arg1={arg1:#x} pid=5 milestone-end\"")
    }
    fn range(offset: u64, length: u64) -> u64 {
        ((offset / 4096) << 32) | (length / 4096)
    }

    fn good() -> Vec<String> {
        vec![
            "wayclick: app-dmabuf probe forged fd sent".into(),
            "uiserver: app-dmabuf import refused reason=NotAppBuffer".into(),
            "wayclick: app-dmabuf probe forged-fd refused".into(),
            ms("app-dmabuf-export-published", 1, range(0, 0x271000)),
            ms("app-dmabuf-export-ack", 1, 0),
            ms("app-dmabuf-allocation-granted", 5, 0x271000),
            "wayclick: app-dmabuf allocated index=0 width=800 height=520 stride=3200 bytes=1664000".into(),
            "uiserver: app-dmabuf import accepted width=800 height=520 stride=3200 size=2560000".into(),
            "uiserver: app-dmabuf snapshot ok bytes=1664000".into(),
            "uiserver: app-dmabuf buffer released".into(),
            "wayclick: app-dmabuf probe passed".into(),
            ms("app-dmabuf-revoke-published", 3, range(0, 0x271000)),
            ms("app-dmabuf-revoke-ack", 3, 0),
            ms("app-dmabuf-range-reclaimed", 0, range(0, 0x271000)),
        ]
    }
    fn join(lines: &[String]) -> String {
        lines.join("\n")
    }

    #[test]
    fn complete_ordered_log_is_accepted_with_its_range() {
        let evidence = validate_app_dmabuf_probe_log(&join(&good())).unwrap();
        assert_eq!(evidence, AppDmabufProbeEvidence { offset: 0, length: 0x271000 });
    }

    #[test]
    fn every_required_marker_is_individually_required() {
        for marker in app_dmabuf_probe_markers() {
            let lines: Vec<String> = good().into_iter().filter(|l| !l.contains(marker)).collect();
            assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err(), "{marker}");
        }
    }

    #[test]
    fn reordered_allocation_chain_is_refused() {
        let mut lines = good();
        lines.swap(3, 4);
        assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err());
    }

    #[test]
    fn provider_failure_wrong_sequence_and_wrong_range_are_refused() {
        let mut lines = good();
        lines[4] = ms("app-dmabuf-export-ack", 1, 22);
        assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err());
        let mut lines = good();
        lines[4] = ms("app-dmabuf-export-ack", 2, 0);
        assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err());
        let mut lines = good();
        lines[13] = ms("app-dmabuf-range-reclaimed", 0, range(0x1000, 0x271000));
        assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err());
        let mut lines = good();
        lines[12] = ms("app-dmabuf-revoke-ack", 3, 5);
        assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err());
        let mut lines = good();
        lines[3] = ms("app-dmabuf-export-published", 1, range(0x1ffff000, 0x2000));
        assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err());
    }

    #[test]
    fn fail_marker_is_extracted_for_fail_fast_and_refused_by_validation() {
        assert_eq!(app_dmabuf_probe_failure(&join(&good())), None);
        let mut lines = good();
        lines.insert(2, "app-dmabuf-probe: FAIL forged fd was imported by the compositor\\n".into());
        assert_eq!(
            app_dmabuf_probe_failure(&join(&lines)).as_deref(),
            Some("app-dmabuf-probe: FAIL forged fd was imported by the compositor")
        );
        assert!(validate_app_dmabuf_probe_log(&join(&lines)).is_err());
    }

    #[test]
    fn flag_arms_graphical_session_and_requires_every_marker() {
        let parsed = parse_smoke_options(["--app-dmabuf-probe".to_string()].into_iter()).unwrap();
        assert!(parsed.app_dmabuf_probe);
        assert!(parsed.gui_dvm_surfaces);
        for marker in app_dmabuf_probe_markers() {
            assert!(parsed.expected_markers.iter().any(|m| m == marker), "{marker}");
        }
        let plain = parse_smoke_options(std::iter::empty::<String>()).unwrap();
        assert!(!plain.app_dmabuf_probe);
        assert!(!plain.expected_markers.iter().any(|m| m.contains("app-dmabuf")));
    }

    #[test]
    fn contract_body_matches_the_client_that_reads_it() {
        let wayclick = include_str!("../../../../apps/wayclick/src/main.rs");
        assert!(wayclick.contains(&format!(
            "const APP_DMABUF_PROBE_CONTRACT_PATH: &str = \"/{PRIVATE_APP_DMABUF_PROBE_CONTRACT_PATH}\";"
        )));
        assert!(wayclick.contains("const APP_DMABUF_PROBE_CONTRACT_BODY: &str = \"contract=rustos-appdmabuf-probe-v1\\n\";"));
        assert_eq!(APP_DMABUF_PROBE_CONTRACT_BODY, b"contract=rustos-appdmabuf-probe-v1\n");
        assert!(wayclick.contains(APP_DMABUF_PROBE_FAIL_MARKER));
    }
}
