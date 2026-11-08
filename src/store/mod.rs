//! Durable state behind the `Repositories` port: SQLite for the daemon, an
//! in-memory store for tests and dry runs.

#[cfg(test)]
pub mod memory;
pub mod sqlite;

#[cfg(test)]
pub use memory::MemoryRepositories;
