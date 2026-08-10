// SPDX-License-Identifier: MIT

/// The ring 3 debug syscall copies at most `USER_DEBUG_CHUNK_BYTES` (256) per
/// pass and gives every chunk its own kernel-owned `user-debug payload=`
/// envelope, so one service line longer than that arrives as several debugcon
/// records - split wherever byte 256 fell, which is usually mid-token.
///
/// Every line-oriented reader below therefore has to put them back together
/// first. `uiserver profile:` is 562 bytes, so its `cursor`, `presented_cursor`,
/// `cursor_moves`, and `background_thread_demotions` fields all landed on a
/// record that no longer carried the prefix; `parse_ui_profile_input_window`
/// returned `None` for every window and the input half of the UI proof could
/// not pass whatever the guest did. The line grew past 256 bytes at some point
/// and took the gate with it silently.
///
/// The producer terminates each service line with a newline, which the envelope
/// escapes, so an escaped trailing newline is the end of a ring 3 line and
/// anything before it is a continuation. The join is bounded so a producer that
/// somehow omits the terminator cannot swallow the rest of the log.
fn rejoin_user_debug_records(log: &str) -> String {
    const PREFIX: &str = "user-debug payload=";
    const ESCAPED_NEWLINE: &str = "\\n";
    const MAX_JOINED_RECORDS: usize = 16;

    let mut out = String::with_capacity(log.len());
    let mut pending: Option<(String, usize)> = None;
    for line in log.lines() {
        match line.strip_prefix(PREFIX) {
            Some(payload) => {
                let (mut joined, count) = pending.take().unwrap_or_else(|| (String::new(), 0));
                joined.push_str(payload);
                if payload.ends_with(ESCAPED_NEWLINE) || count + 1 >= MAX_JOINED_RECORDS {
                    out.push_str(PREFIX);
                    out.push_str(&joined);
                    out.push('\n');
                } else {
                    pending = Some((joined, count + 1));
                }
            }
            None => {
                if let Some((joined, _)) = pending.take() {
                    out.push_str(PREFIX);
                    out.push_str(&joined);
                    out.push('\n');
                }
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if let Some((joined, _)) = pending.take() {
        out.push_str(PREFIX);
        out.push_str(&joined);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod user_debug_record_tests {
    use super::{rejoin_user_debug_records, uiserver_profile_input_pipeline_healthy};

    #[test]
    fn a_profile_line_split_by_the_debug_chunk_boundary_is_rejoined_before_it_is_parsed() {
        // The ring 3 debug syscall copies 256 bytes per pass and envelopes each
        // chunk, so a 562-byte `uiserver profile:` line reaches the log as three
        // records split mid-token. Every field the input gate needs - cursor,
        // presented_cursor, cursor_moves, background_thread_demotions - is past
        // the first boundary, so without rejoining the window parses as None and
        // the gate cannot pass whatever the guest is doing.
        let window = |cursor: &str| {
            let line = format!(
                "uiserver profile: elapsed_ms=1000 frame_hz_milli=66000 loops=500 input_events=60 input_ms=6 input_gap_ms=20 input_last_age_ms=20 input_drops=0 input_slow=0 input_errors=0 cursor_mismatches=0 cursor={cursor} presented_cursor={cursor} background_thread_demotions=13 motion=0 position=0 other=0 backlog=0 cursor_moves=60 wayland_calls=0 wayland_ms=0"
            );
            let mut records = String::new();
            for chunk in line.as_bytes().chunks(256) {
                records.push_str("user-debug payload=");
                records.push_str(std::str::from_utf8(chunk).expect("ascii fixture"));
                records.push('\n');
            }
            // Only the final record carries the producer's line terminator.
            records.pop();
            records.push_str("\\n\n");
            records
        };
        let split: String = ["800,450", "992,450", "992,642"].map(window).join("");
        assert!(split.lines().count() > 3, "fixture must actually be split");
        assert!(
            !uiserver_profile_input_pipeline_healthy(&split, 3, Some(55)),
            "split records must not parse as windows"
        );

        let rejoined = rejoin_user_debug_records(&split);
        assert_eq!(rejoined.lines().count(), 3);
        assert!(uiserver_profile_input_pipeline_healthy(&rejoined, 3, Some(55)));

        // Rejoining must not invent health: the gate still sees a stale cursor.
        assert!(!uiserver_profile_input_pipeline_healthy(
            &rejoin_user_debug_records(&split.replace("presented_cursor=992,642", "presented_cursor=991,642")),
            3,
            Some(55)
        ));
    }

    #[test]
    fn rejoining_leaves_kernel_records_and_terminated_lines_exactly_as_they_were() {
        let log = "seq=1 msg=\"milestone-begin v=1 milestone-end\"\n\
                   user-debug payload=short line\\n\n\
                   seq=2 msg=\"another\"\n";
        assert_eq!(rejoin_user_debug_records(log), log);

        // An unterminated tail is still delivered rather than dropped.
        assert_eq!(
            rejoin_user_debug_records("user-debug payload=no terminator"),
            "user-debug payload=no terminator\n"
        );
    }
}
