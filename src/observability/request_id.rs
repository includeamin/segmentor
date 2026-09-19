//! Request ID generation shared by the HTTP layer and the mapper client.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A process-unique ID: nanoseconds since the epoch plus a sequence number, in hexadecimal.
pub(crate) fn generate() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanoseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanoseconds:x}-{sequence:x}")
}
