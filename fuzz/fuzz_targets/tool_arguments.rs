//! A tool call's arguments, as the model wrote them.
#![no_main]

libfuzzer_sys::fuzz_target!(|raw: &str| {
    let _ = dispatch::parse_arguments(raw);
});
