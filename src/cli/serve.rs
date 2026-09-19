//! The `serve` command: load configuration, start logging, run the HTTP origin.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::http;
use crate::observability::logging;

pub(super) async fn run(arguments: impl Iterator<Item = OsString>) -> Result<()> {
    let options = ServeOptions::parse(arguments)?;
    let config = Config::load(options.config)?;
    let _logging_guard = logging::init(&config.logging)?;
    http::serve(config).await
}

#[derive(Debug)]
pub(super) struct ServeOptions {
    config: PathBuf,
}

impl ServeOptions {
    pub(super) fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Self> {
        let mut config = None;
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--config") => config = arguments.next().map(PathBuf::from),
                _ => return Err(Error::Configuration("unknown serve argument".to_owned())),
            }
        }
        Ok(Self {
            config: config.ok_or_else(|| Error::Configuration("missing --config".to_owned()))?,
        })
    }
}
