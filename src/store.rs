//! SQLite capture index and content-addressed blob files.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store unavailable")]
    Unavailable,
}

pub struct Store;

impl Store {
    pub fn open(_dir: &std::path::Path) -> Result<Self, StoreError> {
        Err(StoreError::Unavailable)
    }
}
