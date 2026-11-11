//! A fetched page, read for its text.
#![no_main]

libfuzzer_sys::fuzz_target!(|html: &str| {
    let _ = tools::readable_text(html);
});
