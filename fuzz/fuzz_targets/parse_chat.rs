//! A model server's reply, as raw bytes.
#![no_main]

libfuzzer_sys::fuzz_target!(|body: &[u8]| {
    let _ = model::parse_chat(body);
});
