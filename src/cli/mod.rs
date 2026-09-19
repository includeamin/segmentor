//! Command-line dispatch.
//!
//! Arguments are parsed by hand: the surface is two commands with a handful of flags, so a
//! parser dependency would cost more than it saves.

mod package;
mod serve;

use std::ffi::OsString;

use crate::APP_NAME;
use crate::error::{Error, Result};

/// Runs the command named by the first argument. With no arguments it prints the program name.
pub(crate) async fn run(mut arguments: impl Iterator<Item = OsString>) -> Result<()> {
    let Some(command) = arguments.next() else {
        println!("{APP_NAME}");
        return Ok(());
    };
    match command.to_str() {
        Some("package") => package::run(arguments).await,
        Some("serve") => serve::run(arguments).await,
        _ => Err(Error::InvalidMedia(
            "expected the `package` or `serve` command".to_owned(),
        )),
    }
}
