pub(crate) mod boxes;
pub(crate) mod codec;
mod edit;
mod fragments;
mod parser;
mod tables;

pub(crate) use parser::{ParsedMedia, parse};
