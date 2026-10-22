//! Where something fetched from GitHub apart from the Inbox stands.

/// Where something being fetched stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched<T> {
    Fetching,
    Done(T),
    /// Why it couldn't be fetched.
    Failed(String),
}

impl<T> Fetched<T> {
    /// What fetching came to: done, or failed with why.
    pub fn from_result(result: anyhow::Result<T>) -> Self {
        match result {
            Ok(fetched) => Self::Done(fetched),
            Err(error) => Self::Failed(format!("{error:#}")),
        }
    }
}
