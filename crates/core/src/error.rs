#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("monetary amount is out of range")]
    MoneyOverflow,
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited { retry_after: std::time::Duration },
    #[error("network request failed: {0}")]
    Network(String),
    #[error("{endpoint} returned HTTP {status}")]
    HttpStatus { endpoint: &'static str, status: u16 },
    #[error("storage operation failed: {0}")]
    Storage(String),
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
