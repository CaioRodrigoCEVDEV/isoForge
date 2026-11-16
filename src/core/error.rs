use std::fmt;

#[derive(Debug, Clone)]
pub struct CoreError {
    pub message: String,
}

impl CoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoreError {}

impl From<std::io::Error> for CoreError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<String> for CoreError {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for CoreError {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

pub type CoreResult<T> = Result<T, CoreError>;
