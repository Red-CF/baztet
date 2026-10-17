//! baztet core: sync pipeline and bazel execution.

pub mod bazel;
pub mod expand;
pub mod sync;
pub mod workspace;

pub use bazel::{BazelError, BazelRunner};