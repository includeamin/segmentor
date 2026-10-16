#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    vodnik::fuzzing::exercise_hevc_slice_header(data);
});
