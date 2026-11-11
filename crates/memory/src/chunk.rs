//! Splitting text by tokens. Embedding limits are in tokens, and the same
//! number of characters can be twice as many tokens in dense text.

/// How notes are split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkConfig {
    /// The largest chunk, in estimated tokens.
    pub max_tokens: u32,
    /// How much each chunk repeats of the one before, in estimated tokens.
    pub overlap_tokens: u32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_tokens: 512,
            overlap_tokens: 48,
        }
    }
}

/// One chunk's text and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// The text.
    pub text: String,
    /// Its estimated tokens.
    pub tokens: u32,
}

/// Characters per token within a run of letters and digits.
const CHARS_PER_WORD_TOKEN: u32 = 3;

/// Estimates tokens without the embedder's tokenizer, which the wire format
/// doesn't expose. Punctuation and non-ASCII count one each, and letters and
/// digits one per three. A real embedder measured about 3.2 characters per
/// token on prose and 1.8 on dense JSON; this matches prose and errs high on
/// punctuation-heavy text, which only makes the chunks smaller.
#[must_use]
pub fn estimate_tokens(text: &str) -> u32 {
    let mut tokens = 0u32;
    let mut run = 0u32;
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            run += 1;
            continue;
        }
        tokens = tokens.saturating_add(run.div_ceil(CHARS_PER_WORD_TOKEN));
        run = 0;
        if !character.is_whitespace() {
            tokens = tokens.saturating_add(1);
        }
    }
    tokens.saturating_add(run.div_ceil(CHARS_PER_WORD_TOKEN))
}

fn is_blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// Cuts text with no usable boundary into pieces that fit.
fn hard_split(text: &str, max_tokens: u32) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        current.push(character);
        if estimate_tokens(&current) > max_tokens {
            current.pop();
            pieces.push(std::mem::take(&mut current));
            current.push(character);
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Splits on each separator in turn, keeping every character.
fn atoms(text: &str, max_tokens: u32, separators: &[&dyn Fn(char) -> bool]) -> Vec<String> {
    if estimate_tokens(text) <= max_tokens {
        return vec![text.to_owned()];
    }
    let Some((separator, rest)) = separators.split_first() else {
        return hard_split(text, max_tokens);
    };
    text.split_inclusive(*separator)
        .flat_map(|part| atoms(part, max_tokens, rest))
        .collect()
}

/// A paragraph break is a newline. Sentences end in punctuation.
fn split_into_atoms(text: &str, max_tokens: u32) -> Vec<String> {
    let newline = |c: char| c == '\n';
    let sentence_end = |c: char| matches!(c, '.' | '!' | '?' | ';');
    let word_end = |c: char| c.is_whitespace();
    atoms(text, max_tokens, &[&newline, &sentence_end, &word_end])
}

/// The atoms at the end of a chunk that fit in the overlap.
fn overlap_tail(chunk_atoms: &[String], overlap_tokens: u32) -> Vec<String> {
    let mut tail = Vec::new();
    let mut tokens = 0u32;
    for atom in chunk_atoms.iter().rev() {
        tokens = tokens.saturating_add(estimate_tokens(atom));
        if tokens > overlap_tokens {
            break;
        }
        tail.push(atom.clone());
    }
    tail.reverse();
    tail
}

fn piece_of(atoms: &[String]) -> Piece {
    let text = atoms.concat();
    Piece {
        tokens: estimate_tokens(&text),
        text,
    }
}

/// Splits text into pieces of at most `config.max_tokens`, breaking at the
/// largest boundary that fits. Blank pieces are dropped: an embedder answers
/// an empty input with an empty vector and no error.
#[must_use]
pub fn chunk_text(text: &str, config: ChunkConfig) -> Vec<Piece> {
    let max = config.max_tokens.max(1);
    let overlap = config.overlap_tokens.min(max / 2);
    let mut pieces = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut tokens = 0u32;
    let mut fresh = 0usize;
    for atom in split_into_atoms(text, max) {
        let size = estimate_tokens(&atom);
        if tokens.saturating_add(size) > max && fresh > 0 {
            pieces.push(piece_of(&current));
            current = overlap_tail(&current, overlap);
            tokens = current.iter().map(|a| estimate_tokens(a)).sum();
            fresh = 0;
            // An overlap too big to sit beside this atom is dropped.
            if tokens.saturating_add(size) > max {
                current.clear();
                tokens = 0;
            }
        }
        tokens = tokens.saturating_add(size);
        current.push(atom);
        fresh += 1;
    }
    if fresh > 0 {
        pieces.push(piece_of(&current));
    }
    pieces.retain(|piece| !is_blank(&piece.text));
    pieces
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "a test on small texts"
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SMALL: ChunkConfig = ChunkConfig {
        max_tokens: 20,
        overlap_tokens: 0,
    };

    fn without_space(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn the_estimate_tracks_prose_and_dense_text() {
        let prose = "The quick brown fox jumps over the lazy dog and keeps on running. ".repeat(20);
        let per_token = prose.len() as f64 / f64::from(estimate_tokens(&prose));
        assert!((2.8..4.5).contains(&per_token), "{per_token}");
        let dense = r#"{"url":"https://a.b/c?d=1&e=2","id":[1,2,3],"x":{"y":"z"}}"#.repeat(20);
        let per_token = dense.len() as f64 / f64::from(estimate_tokens(&dense));
        assert!((1.0..2.0).contains(&per_token), "{per_token}");
    }

    #[test]
    fn a_short_note_is_one_chunk() {
        assert_eq!(chunk_text("hello there", SMALL).len(), 1);
    }

    #[test]
    fn blank_text_makes_no_chunks() {
        assert_eq!(chunk_text("  \n\n \t", SMALL).len(), 0);
        assert_eq!(chunk_text("", SMALL).len(), 0);
    }

    #[test]
    fn it_breaks_at_paragraphs_before_sentences() {
        let text = format!(
            "{}\n{}",
            "one two three four five.", "six seven eight nine ten."
        );
        let config = ChunkConfig {
            max_tokens: 14,
            overlap_tokens: 0,
        };
        let pieces = chunk_text(&text, config);
        assert_eq!(pieces.len(), 2);
        assert!(pieces[0].text.ends_with(".\n"), "{:?}", pieces[0].text);
    }

    #[test]
    fn a_word_too_long_for_any_chunk_is_cut() {
        let pieces = chunk_text(&"a".repeat(1000), SMALL);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|piece| piece.tokens <= 20));
    }

    #[test]
    fn multibyte_text_is_cut_on_character_boundaries() {
        let pieces = chunk_text(&"日本語".repeat(100), SMALL);
        assert!(pieces.iter().all(|piece| piece.tokens <= 20));
    }

    #[test]
    fn chunks_overlap_when_asked() {
        let text = "alpha one two. bravo three four. charlie five six. delta seven eight.";
        let config = ChunkConfig {
            max_tokens: 16,
            overlap_tokens: 8,
        };
        let pieces = chunk_text(text, config);
        assert!(pieces.len() > 1);
        assert!(pieces[1].text.contains("bravo"), "{pieces:?}");
        assert!(pieces[0].text.contains("bravo"), "{pieces:?}");
    }

    fn any_text() -> impl Strategy<Value = String> {
        prop_oneof!["[a-z .\n]{0,600}", "\\PC{0,400}", "[{}\":,0-9a-f]{0,600}",]
    }

    proptest! {
        #[test]
        fn no_chunk_is_over_the_limit(text in any_text(), max in 4u32..80, overlap in 0u32..40) {
            let config = ChunkConfig { max_tokens: max, overlap_tokens: overlap };
            for piece in chunk_text(&text, config) {
                prop_assert!(piece.tokens <= max, "{} > {max}: {:?}", piece.tokens, piece.text);
                prop_assert!(!is_blank(&piece.text));
            }
        }

        #[test]
        fn nothing_is_lost_without_overlap(text in any_text(), max in 4u32..80) {
            let config = ChunkConfig { max_tokens: max, overlap_tokens: 0 };
            let joined: String = chunk_text(&text, config).iter().map(|p| p.text.as_str()).collect();
            prop_assert_eq!(without_space(&joined), without_space(&text));
        }

        #[test]
        fn overlap_loses_nothing_either(text in "([a-z]{1,6}[ .\n]){0,120}", max in 8u32..60) {
            let config = ChunkConfig { max_tokens: max, overlap_tokens: 4 };
            let joined: String = chunk_text(&text, config).iter().map(|p| p.text.as_str()).collect();
            for word in text.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()) {
                prop_assert!(joined.contains(word), "{word}");
            }
        }
    }
}
