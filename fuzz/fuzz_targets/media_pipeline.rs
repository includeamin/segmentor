#![no_main]

use std::io::Write;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() as u64 > segmentor::fuzzing::max_input_bytes() {
        return;
    }

    let mut file = tempfile::NamedTempFile::new().expect("temporary file should be available");
    file.write_all(data)
        .expect("temporary fuzz input should be writable");
    segmentor::fuzzing::exercise_media_pipeline(file.path());
});
