//! What went wrong at a JMAP server, as kinds the adapter maps onto the
//! backend errors every provider shares. The words are for the log;
//! whatever a person reads is written in `mailrs-sync` or the app, through
//! the translation catalogue.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JmapError {
    /// Status 401: the server no longer takes the token, which the person
    /// revoked or which expired.
    #[error("the server refused the token; the account must sign in again")]
    Unauthorized,
    #[error("network error: {0}")]
    Network(String),
    /// Status 429 or 503, or a `rateLimit` method error.
    #[error("the server asked to slow down")]
    Throttled { retry_after: Option<Duration> },
    /// A method's `notFound`, or a blob the server does not hold.
    #[error("not found")]
    NotFound,
    /// `cannotCalculateChanges`: the server no longer keeps changes since
    /// that state, and the caller lists again.
    #[error("the server cannot tell what changed since that state")]
    StateLost,
    /// `stateMismatch`: an `ifInState` that is no longer the state.
    #[error("it changed on the server first")]
    StateMismatch,
    /// The session names no account with the capability a call needs,
    /// such as mail or submission.
    #[error("the account offers no {0}")]
    Missing(&'static str),
    /// A method error of another type, with the server's description.
    #[error("{kind}: {description}")]
    Method { kind: String, description: String },
    /// A request-level problem (RFC 7807) or an unexpected status.
    #[error("HTTP {status}: {detail}")]
    Http { status: u16, detail: String },
    #[error("the answer is larger than {limit} bytes")]
    TooLarge { limit: usize },
    /// A URL the session named that the token must not go to: not HTTPS,
    /// and not this computer.
    #[error("refused to send the token to {0}")]
    Insecure(String),
    #[error("could not read the server's answer: {0}")]
    Decode(String),
}

impl JmapError {
    pub fn is_transient(&self) -> bool {
        matches!(self, JmapError::Network(_) | JmapError::Throttled { .. })
    }

    /// The error a method answered with, from its `type` and
    /// `description`.
    pub fn method(kind: &str, description: &str) -> JmapError {
        match kind {
            "cannotCalculateChanges" => JmapError::StateLost,
            "stateMismatch" => JmapError::StateMismatch,
            "notFound" => JmapError::NotFound,
            "rateLimit" | "tooManyRequests" => JmapError::Throttled { retry_after: None },
            _ => JmapError::Method {
                kind: kind.to_string(),
                description: description.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::JmapError;

    #[test]
    fn method_errors_take_the_kinds_the_adapter_acts_on() {
        assert_eq!(JmapError::method("cannotCalculateChanges", ""), JmapError::StateLost);
        assert_eq!(JmapError::method("stateMismatch", ""), JmapError::StateMismatch);
        assert!(JmapError::method("rateLimit", "").is_transient());
        assert_eq!(
            JmapError::method("invalidArguments", "bad filter"),
            JmapError::Method {
                kind: "invalidArguments".into(),
                description: "bad filter".into()
            }
        );
    }
}
