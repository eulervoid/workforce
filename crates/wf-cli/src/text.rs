//! Measuring, cutting and timing text the same way in the CLI, the transcript and the dashboard.

/// Columns `text` takes on a terminal.
pub fn display_width(text: &str) -> usize {
    textwrap::core::display_width(text)
}

/// The longest start of `text` that takes at most `room` columns.
pub fn prefix_within(text: &str, room: usize) -> &str {
    let mut used = 0;
    for (index, character) in text.char_indices() {
        used += display_width(character.encode_utf8(&mut [0; 4]));
        if used > room {
            return &text[..index];
        }
    }
    text
}

/// `text` cut to `room` columns, ending in `…` when anything was cut.
pub fn cut(text: &str, room: usize) -> String {
    if display_width(text) <= room {
        return text.to_owned();
    }
    format!(
        "{}…",
        prefix_within(text, room.saturating_sub(1)).trim_end()
    )
}

/// `0.4s`, `14m`, `3h05m`, `2d`.
pub fn duration(millis: u64) -> String {
    let minutes = millis / 60_000;
    match minutes {
        0 => format!("{}.{}s", millis / 1000, millis % 1000 / 100),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h{:02}m", minutes / 60, minutes % 60),
        _ => format!("{}d", minutes / 1440),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_text_ends_in_an_ellipsis_within_its_room() {
        assert_eq!(cut("short", 5), "short");
        assert_eq!(cut("a longer text", 8), "a longe…");
        assert_eq!(cut("日本語のテキスト", 7), "日本語…");
        assert_eq!(display_width(&cut("日本語のテキスト", 8)), 7);
    }

    #[test]
    fn durations_grow_from_tenths_of_seconds_to_days() {
        assert_eq!(duration(400), "0.4s");
        assert_eq!(duration(14 * 60_000), "14m");
        assert_eq!(duration(185 * 60_000), "3h05m");
        assert_eq!(duration(2 * 86_400_000), "2d");
    }
}
