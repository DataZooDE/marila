//! Source-stage abstractions. Each `Source` emits `RawDoc`s; the pipeline
//! drains them into the parse stage.

pub mod local;
pub mod types;

pub use types::*;
