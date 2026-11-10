//! A model's text, searched for a JSON object.
#![no_main]

libfuzzer_sys::fuzz_target!(|text: &str| {
    let _ = model::first_json_object(text);
});
