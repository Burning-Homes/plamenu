//! A mutation may own its transaction or join a larger atomic operation.
//! Borrowed connections never commit or publish post-commit side effects.
use plamenu_db::{DbError, PgConnection, PgPool};
use std::ops::{Deref, DerefMut};

pub(crate) enum Transaction<'a> {
    Owned(sqlx::Transaction<'a, sqlx::Postgres>),
    Borrowed(&'a mut PgConnection),
}
impl<'a> Transaction<'a> {
    pub(crate) async fn new(
        pool: &PgPool,
        connection: Option<&'a mut PgConnection>,
    ) -> Result<Self, DbError> {
        match connection {
            Some(conn) => Ok(Self::Borrowed(conn)),
            None => Ok(Self::Owned(pool.begin().await?)),
        }
    }
    /// True only when this scope committed. Its caller owns notifications.
    pub(crate) async fn commit(self) -> Result<bool, DbError> {
        match self {
            Self::Owned(tx) => {
                tx.commit().await?;
                Ok(true)
            }
            Self::Borrowed(_) => Ok(false),
        }
    }
}
impl Deref for Transaction<'_> {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(tx) => tx,
            Self::Borrowed(conn) => conn,
        }
    }
}
impl DerefMut for Transaction<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Owned(tx) => tx,
            Self::Borrowed(conn) => conn,
        }
    }
}
