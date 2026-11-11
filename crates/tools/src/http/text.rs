//! Readable text from HTML. It reads tokens and builds no tree, so its work
//! grows with the page's length and hostile nesting can't exhaust the stack.

use std::cell::RefCell;

use html5ever::TokenizerResult;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

use crate::clamp::clamp_chars;

/// The longest title kept.
const MAX_TITLE: usize = 512;

/// Elements whose text isn't what the page is about.
const SKIPPED: [&str; 10] = [
    "script", "style", "noscript", "template", "svg", "nav", "footer", "aside", "iframe", "object",
];

/// Elements that start a new line.
const BLOCKS: [&str; 29] = [
    "address",
    "article",
    "blockquote",
    "br",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "figcaption",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "td",
    "th",
    "tr",
    "ul",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Gap {
    None,
    Space,
    Line,
}

#[derive(Debug)]
struct Extract {
    text: String,
    title: Option<String>,
    in_title: bool,
    gap: Gap,
    /// The element being skipped, and how many of it are open.
    skipping: Option<(String, u32)>,
}

impl Extract {
    fn widen(&mut self, gap: Gap) {
        self.gap = self.gap.max(gap);
    }

    fn push_text(&mut self, chunk: &str) {
        if chunk.starts_with(char::is_whitespace) {
            self.widen(Gap::Space);
        }
        for word in chunk.split_whitespace() {
            if !self.text.is_empty() {
                match self.gap {
                    Gap::None => {}
                    Gap::Space => self.text.push(' '),
                    Gap::Line => self.text.push('\n'),
                }
            }
            self.text.push_str(word);
            self.gap = Gap::Space;
        }
        self.gap = if chunk.ends_with(char::is_whitespace) || chunk.trim().is_empty() {
            self.gap.max(Gap::Space)
        } else {
            Gap::None
        };
    }

    fn characters(&mut self, chunk: &str) {
        if self.in_title {
            if let Some(title) = &mut self.title {
                title.push_str(chunk);
                clamp_chars(title, MAX_TITLE);
            }
        } else if self.skipping.is_none() {
            self.push_text(chunk);
        }
    }

    /// Updates the skip state. True if `tag` is inside something skipped, or
    /// starts it.
    fn track_skip(&mut self, tag: &Tag) -> bool {
        let name: &str = &tag.name;
        if let Some((skipped, open)) = &mut self.skipping {
            if skipped == name && !tag.self_closing {
                match tag.kind {
                    TagKind::StartTag => *open = open.saturating_add(1),
                    TagKind::EndTag => *open = open.saturating_sub(1),
                }
            }
            if *open == 0 {
                self.skipping = None;
            }
            return true;
        }
        if tag.kind == TagKind::StartTag && !tag.self_closing && SKIPPED.contains(&name) {
            self.skipping = Some((name.to_owned(), 1));
        }
        self.skipping.is_some()
    }

    fn tag(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        let name: &str = &tag.name;
        let skipped = self.track_skip(tag);
        if !skipped {
            if BLOCKS.contains(&name) {
                self.widen(Gap::Line);
            }
            if name == "title" {
                let opening = tag.kind == TagKind::StartTag && !tag.self_closing;
                if opening && self.title.is_none() {
                    self.title = Some(String::new());
                }
                self.in_title = opening && self.title.as_ref().is_some_and(String::is_empty);
            }
        }
        if tag.kind != TagKind::StartTag || tag.self_closing {
            return TokenSinkResult::Continue;
        }
        // Without a tree builder, the tokenizer has to be told that these
        // elements' contents aren't markup.
        match name {
            "script" => TokenSinkResult::RawData(RawKind::ScriptData),
            "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
                TokenSinkResult::RawData(RawKind::Rawtext)
            }
            "title" | "textarea" => TokenSinkResult::RawData(RawKind::Rcdata),
            "plaintext" => TokenSinkResult::Plaintext,
            _ => TokenSinkResult::Continue,
        }
    }
}

#[derive(Debug)]
struct Sink(RefCell<Extract>);

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut extract = self.0.borrow_mut();
        match token {
            Token::TagToken(tag) => extract.tag(&tag),
            Token::CharacterTokens(chunk) => {
                extract.characters(&chunk);
                TokenSinkResult::Continue
            }
            _ => TokenSinkResult::Continue,
        }
    }
}

/// The page's title, if it has one, and its readable text.
#[must_use]
pub fn readable_text(html: &str) -> (Option<String>, String) {
    let sink = Sink(RefCell::new(Extract {
        text: String::new(),
        title: None,
        in_title: false,
        gap: Gap::None,
        skipping: None,
    }));
    let tokenizer = Tokenizer::new(sink, TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(html));
    while let TokenizerResult::Script(()) = tokenizer.feed(&input) {}
    tokenizer.end();
    let extract = tokenizer.sink.0.into_inner();
    let title = extract
        .title
        .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|title| !title.is_empty());
    (title, extract.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn text(html: &str) -> String {
        readable_text(html).1
    }

    #[test]
    fn it_keeps_the_words_and_finds_the_title() {
        let (title, body) = readable_text(
            "<html><head><title> A  page </title></head>\
             <body><h1>Hello</h1><p>one <b>two</b>&amp;three</p></body></html>",
        );
        assert_eq!(title.as_deref(), Some("A page"));
        assert_eq!(body, "Hello\none two&three");
    }

    #[test]
    fn words_split_by_inline_markup_stay_whole() {
        assert_eq!(text("<p>un<i>break</i>able</p>"), "unbreakable");
    }

    #[test]
    fn what_isnt_content_is_dropped() {
        let body = text(
            "<script>var a='<p>no</p>';</script><style>p{}</style>\
             <nav><nav>x</nav>menu</nav><p>kept</p>\
             <svg><title>icon</title><text>no</text></svg><footer>no</footer><p>too</p>",
        );
        assert_eq!(body, "kept\ntoo");
    }

    #[test]
    fn a_title_in_an_image_isnt_the_pages() {
        let (title, _) = readable_text("<svg><title>icon</title></svg><title>real</title>");
        assert_eq!(title.as_deref(), Some("real"));
    }

    #[test]
    fn markup_inside_raw_elements_stays_inert() {
        assert_eq!(text("<script><!-- </script><p>kept</p>"), "kept");
        assert_eq!(text("<style><!-- </style><p>kept</p>"), "kept");
        let (title, body) = readable_text("<title><!-- a</title><p>kept</p>");
        assert_eq!(title.as_deref(), Some("<!-- a"));
        assert_eq!(body, "kept");
    }

    #[test]
    fn hostile_nesting_is_only_more_tokens() {
        let body = format!("{}deep", "<div>".repeat(200_000));
        assert_eq!(text(&body), "deep");
    }

    #[test]
    fn an_unclosed_skip_ends_at_the_end_of_the_page() {
        assert_eq!(text("<p>a</p><script>never closed"), "a");
    }

    proptest! {
        #[test]
        fn any_text_comes_back_without_panicking(html in ".{0,400}") {
            let (_, body) = readable_text(&html);
            prop_assert!(!body.starts_with(char::is_whitespace));
        }
    }
}
