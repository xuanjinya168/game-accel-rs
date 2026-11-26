use std::fmt;

/// Failures of a client session that the caller must tell apart.
/// The text is for display only; retry policy matches the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The server rejected the token. Retrying cannot succeed.
    AuthRejected { detail: String },
    /// The local proxy port is already taken. Retrying cannot succeed.
    PortInUse { detail: String },
    /// Handshake, network, or tunnel failure. Retrying may succeed.
    Other(String),
}

impl ConnectError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Other(_))
    }

    pub fn message(&self) -> &str {
        match self {
            Self::AuthRejected { detail } | Self::PortInUse { detail } | Self::Other(detail) => {
                detail
            }
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ConnectError {}

/// Recover a [`ConnectError`] from an `anyhow` chain. Unknown failures stay retryable.
pub fn classify(err: &anyhow::Error) -> ConnectError {
    for cause in err.chain() {
        if let Some(found) = cause.downcast_ref::<ConnectError>() {
            return found.clone();
        }
    }
    ConnectError::Other(format!("{err:#}"))
}
