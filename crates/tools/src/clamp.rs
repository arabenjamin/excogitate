//! Cutting text to a schema's maximum.

/// Cuts `text` to at most `max` characters. True if anything was cut.
pub(crate) fn clamp_chars(text: &mut String, max: usize) -> bool {
    match text.char_indices().nth(max) {
        Some((end, _)) => {
            text.truncate(end);
            true
        }
        None => false,
    }
}

/// `text`, at most `max` characters long.
pub(crate) fn clamped(mut text: String, max: usize) -> String {
    clamp_chars(&mut text, max);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_counts_characters_not_bytes() {
        let mut text = "ééé".to_owned();
        assert!(clamp_chars(&mut text, 2));
        assert_eq!(text, "éé");
        assert!(!clamp_chars(&mut text, 2));
        assert_eq!(text, "éé");
    }
}
