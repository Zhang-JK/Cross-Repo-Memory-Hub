use memhub_store::StoreError;

pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("not identified: call register_agent (first time) or connect_agent before using this tool")]
    NotIdentified,

    #[error("this connection is already identified as agent '{0}'")]
    AlreadyIdentified(String),

    #[error("agent '{0}' is already registered; use connect_agent with its token")]
    AgentExists(String),

    #[error("unknown agent '{0}'; call register_agent first")]
    UnknownAgent(String),

    #[error("authentication failed for agent '{0}'")]
    AuthFailed(String),

    #[error("permission denied: {0}")]
    Forbidden(String),

    #[error("unknown repo '{0}'; call add_repo to register it")]
    UnknownRepo(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// Stable machine-readable category.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Store(StoreError::NotFound(_)) => "not_found",
            AppError::Store(StoreError::AlreadyExists(_)) => "already_exists",
            AppError::Store(StoreError::VersionConflict { .. }) => "version_conflict",
            AppError::Store(
                StoreError::InvalidName { .. } | StoreError::InvalidId(_) | StoreError::Invalid(_),
            ) => "invalid_input",
            AppError::Store(_) => "storage_error",
            AppError::NotIdentified => "not_identified",
            AppError::AlreadyIdentified(_) => "already_identified",
            AppError::AgentExists(_) => "agent_exists",
            AppError::UnknownAgent(_) => "unknown_agent",
            AppError::AuthFailed(_) => "auth_failed",
            AppError::Forbidden(_) => "forbidden",
            AppError::UnknownRepo(_) => "unknown_repo",
            AppError::Invalid(_) => "invalid_input",
            AppError::Internal(_) => "internal",
        }
    }
}
