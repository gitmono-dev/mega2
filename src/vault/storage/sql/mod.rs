//! This module includes storage backends for all SQL types. Currently supported: SQLite, PostgreSQL.

#[cfg(any())]
pub mod postgresql;
#[cfg(any())]
pub mod sqlite;
