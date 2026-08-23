//! The AMM module server crate.
//!
//! Scaffolding: only the database schema (spec §5) exists so far. `db.rs`
//! is exercised directly by its own tests; the `ServerModule` impl, audit,
//! and API endpoints land in later tasks.

pub mod db;
