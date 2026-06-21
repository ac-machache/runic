//! Fuzz the HTML→text pass: it must never panic on arbitrary input.
//! (The char-boundary panic in entity decoding lived right here.)
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = runic_tools::html_to_text(data);
});
