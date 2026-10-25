//! baztet core: sync pipeline and bazel execution.

pub mod aspect;
pub mod bazel;
pub mod bench;
pub mod capture;
pub mod exec;
pub mod expand;
pub mod ideinfo;
pub mod junit;
pub mod query;
pub mod recipe;
pub mod reuse;
pub mod sync;
pub mod workspace;

pub use bazel::{BazelError, BazelRunner};
pub use baztet_bep;
pub use baztet_projectview::ProjectView;
pub use reuse::{Reuse, SyncReport};