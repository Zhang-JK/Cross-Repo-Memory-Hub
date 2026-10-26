use memhub_store::StoreError;

pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(
        "not identified: this MCP session has no agent identity. Call connect_agent with your agent \
         name, then retry. To have every session identified automatically, set the X-Memhub-Agent \
         header in the MCP client config"
    )]
    NotIdentified,

    #[error("this connection is already identified as agent '{0}'")]
    AlreadyIdentified(String),

    #[error("agent '{0}' does not exist yet; pass 'role' to create it")]
    UnknownAgent(String),

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
            AppError::UnknownAgent(_) => "unknown_agent",
            AppError::Forbidden(_) => "forbidden",
            AppError::UnknownRepo(_) => "unknown_repo",
            AppError::Invalid(_) => "invalid_input",
            AppError::Internal(_) => "internal",
        }
    }
}
