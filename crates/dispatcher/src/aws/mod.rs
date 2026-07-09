//! AWS seams: one domain-level trait per service, plus the error split every
//! degradation path in this binary hinges on.

pub mod microvm;
pub mod params;
pub mod secretsman;

/// The one load-bearing error distinction: an API error response is
/// degradable and code-matchable; a transport failure must fail loud.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AwsApiError {
    /// The API returned an error response: degradable, code-matchable.
    #[error("{message}")]
    Service {
        code: Option<String>,
        message: String,
    },
    /// Transport/SDK failure: never swallowed by degradation paths.
    #[error("{0}")]
    Transport(String),
}

impl AwsApiError {
    pub fn is_service(&self) -> bool {
        matches!(self, AwsApiError::Service { .. })
    }

    pub fn is_code(&self, code: &str) -> bool {
        matches!(self, AwsApiError::Service { code: Some(c), .. } if c == code)
    }
}

/// Swallow service errors, propagate transport errors — for best-effort
/// cleanup calls (terminate/delete) where an API "no" is acceptable.
pub fn ignore_service(r: Result<(), AwsApiError>) -> Result<(), AwsApiError> {
    match r {
        Err(e) if e.is_service() => Ok(()),
        other => other,
    }
}

/// Map an SDK error preserving the service/transport split. The `SdkError`
/// machinery is identical across service crates; `aws_sdk_ssm`'s re-exports
/// serve as the shared spelling.
pub(crate) fn map_sdk_err<E, R>(op: &str, e: aws_sdk_ssm::error::SdkError<E, R>) -> AwsApiError
where
    E: aws_sdk_ssm::error::ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    match &e {
        aws_sdk_ssm::error::SdkError::ServiceError(ctx) => {
            let meta = ctx.err().meta();
            let code = meta.code().map(str::to_string);
            let message = format!(
                "{op} failed ({}): {}",
                code.as_deref().unwrap_or("Unknown"),
                meta.message().unwrap_or("")
            );
            AwsApiError::Service { code, message }
        }
        other => AwsApiError::Transport(format!(
            "{}",
            aws_sdk_ssm::error::DisplayErrorContext(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(code: &str) -> AwsApiError {
        AwsApiError::Service {
            code: Some(code.to_string()),
            message: format!("({code})"),
        }
    }

    #[test]
    fn code_matching_only_applies_to_service_errors() {
        assert!(service("AccessDeniedException").is_code("AccessDeniedException"));
        assert!(!service("Other").is_code("AccessDeniedException"));
        assert!(!AwsApiError::Transport("boom".into()).is_code("AccessDeniedException"));
    }

    #[test]
    fn ignore_service_swallows_only_service_errors() {
        assert!(ignore_service(Ok(())).is_ok());
        assert!(ignore_service(Err(service("Denied"))).is_ok());
        assert!(ignore_service(Err(AwsApiError::Transport("boom".into()))).is_err());
    }
}
