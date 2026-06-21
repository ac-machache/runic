//! Fuzz the calculator's recursive-descent evaluator: arbitrary input must
//! return an error, never panic (no index/overflow/recursion crash).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = runic_tools::eval_calc(data);
});
