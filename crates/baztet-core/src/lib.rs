//! baztet core: sync pipeline and bazel execution.

pub mod aspect;
pub mod bazel;
pub mod bench;
pub mod exec;
pub mod expand;
pub mod ideinfo;
pub mod sync;
pub mod workspace;

pub use bazel::{BazelError, BazelRunner};
