use core::fmt;
use core::fmt::Write as _;

use super::{
    CurrentUserLogContext, FixedDebugconLine, MilestoneRecord, SYNTHETIC_WARNING_MODULE_PATH,
    fnv1a64,
};

const MILESTONE_FRAME_PREFIX: &str = "milestone-begin v=1 ";
const MILESTONE_CHECKSUM_PREFIX: &str = " checksum=";
const MILESTONE_FRAME_SUFFIX: &str = " milestone-end\"\r\n";

fn write_optional_debug_id<const N: usize>(
    line: &mut FixedDebugconLine<N>,
    value: Option<u64>,
) -> fmt::Result {
    match value {
        Some(value) => write!(line, "{value}"),
        None => line.write_str("-"),
    }
}

/// Renders one self-framing milestone record into a fixed buffer.
///
/// The checksum covers the complete canonical payload between
/// `milestone-begin` and `checksum=`. The outer structured-log fields repeat
/// those values for existing readers, while the inner frame lets evidence
/// consumers reject byte interleaving or a partial line without guessing.
pub(super) fn render_milestone_debugcon_line<const N: usize>(
    line: &mut FixedDebugconLine<N>,
    output_seq: u64,
    record: MilestoneRecord,
    user_context: Option<CurrentUserLogContext>,
    milestones_dropped: u64,
    discarded_bytes: u64,
) -> fmt::Result {
    write!(
        line,
        "seq={} ts_us={} tick={} lvl=info cat={} mod={} line=0 pid=",
        output_seq,
        record.ts_us,
        record.tick,
        record.category.as_str(),
        SYNTHETIC_WARNING_MODULE_PATH,
    )?;
    write_optional_debug_id(line, user_context.map(|context| context.process_id))?;
    line.write_str(" tid=")?;
    write_optional_debug_id(line, user_context.map(|context| context.thread_id))?;
    line.write_str(" msg=\"")?;

    let semantic_start = line.len();
    write!(
        line,
        "{}output_seq={} seq={} ts_us={} tick={} cat={} name={} arg0={:#x} arg1={:#x} pid=",
        MILESTONE_FRAME_PREFIX,
        output_seq,
        record.seq,
        record.ts_us,
        record.tick,
        record.category.as_str(),
        record.name,
        record.arg0,
        record.arg1,
    )?;
    write_optional_debug_id(line, user_context.map(|context| context.process_id))?;
    line.write_str(" tid=")?;
    write_optional_debug_id(line, user_context.map(|context| context.thread_id))?;
    write!(
        line,
        " dropped={} discarded_bytes={}",
        milestones_dropped, discarded_bytes
    )?;
    let checksum = fnv1a64(&line.bytes()[semantic_start..]);
    write!(line, "{MILESTONE_CHECKSUM_PREFIX}{checksum:016x}")?;
    line.write_str(MILESTONE_FRAME_SUFFIX)
}

#[cfg(test)]
pub(super) fn find_debugcon_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

#[cfg(test)]
fn decode_fixed_hex_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != 16 {
        return None;
    }
    bytes.iter().try_fold(0_u64, |value, byte| {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => return None,
        };
        Some((value << 4) | u64::from(nibble))
    })
}

#[cfg(test)]
pub(super) fn parse_milestone_debugcon_checksum(line: &[u8]) -> Option<(usize, usize, u64)> {
    let semantic_start = find_debugcon_bytes(line, MILESTONE_FRAME_PREFIX.as_bytes())?;
    let checksum_offset = semantic_start
        + find_debugcon_bytes(
            &line[semantic_start..],
            MILESTONE_CHECKSUM_PREFIX.as_bytes(),
        )?;
    let checksum_start = checksum_offset + MILESTONE_CHECKSUM_PREFIX.len();
    let checksum_end = checksum_start.checked_add(16)?;
    let suffix_end = checksum_end.checked_add(MILESTONE_FRAME_SUFFIX.len())?;
    if suffix_end != line.len()
        || line.get(checksum_end..suffix_end) != Some(MILESTONE_FRAME_SUFFIX.as_bytes())
    {
        return None;
    }
    let expected_checksum = line
        .get(checksum_start..checksum_end)
        .and_then(decode_fixed_hex_u64)?;
    Some((semantic_start, checksum_offset, expected_checksum))
}
