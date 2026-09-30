#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    segmentor::fuzzing::exercise_avc_slice_header(data);
});
