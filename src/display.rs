//! Safe rendering of untrusted catalog text in terminal interfaces.

/// Replaces every Unicode control character with an inert visible escape.
///
/// Catalog content is provider-derived and may contain terminal escape
/// sequences. Keeping controls visible rather than emitting them prevents a
/// title, transcript, or metadata field from changing terminal state.
#[must_use]
pub fn visible_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                format!("\\u{{{:04X}}}", u32::from(character))
            } else {
                character.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::visible_text;

    #[test]
    fn makes_c0_and_c1_controls_inert_and_visible() {
        assert_eq!(
            visible_text("before\u{1b}]0;title\u{7}\u{9b}after"),
            "before\\u{001B}]0;title\\u{0007}\\u{009B}after"
        );
    }
}
