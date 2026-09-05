//! Turns a three-level fleet YAML file into a fully-resolved `FleetSpec`
//! (spec §5). Pure apart from reading the file and host defaults.

pub mod error;
pub mod merge;

pub use error::ConfigError;
pub use merge::{merge, merge_layers, strip_nulls};
