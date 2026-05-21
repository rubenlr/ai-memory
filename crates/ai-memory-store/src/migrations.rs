//! refinery-driven schema migrations.

refinery::embed_migrations!("migrations");

/// Run all pending migrations against an open connection.
///
/// # Errors
/// Propagates the underlying refinery error if a migration fails.
pub fn run(conn: &mut rusqlite::Connection) -> Result<(), refinery::Error> {
    migrations::runner().run(conn)?;
    Ok(())
}
