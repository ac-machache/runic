//! Fuzz HTML entity decoding — the exact spot that panicked on a `&` followed
//! by multi-byte UTF-8 with no nearby `;`. Must never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = runic_tools::decode_entities(data);
});
