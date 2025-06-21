use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("GTK application failed to initialize: {0}")]
    GtkInitialization(String),

    #[error("Failed to create tokio runtime: {0}")]
    TokioRuntime(String),

    #[error("CSS provider failed to load stylesheet: {0}")]
    CssLoad(String),

    #[error("Layer shell initialization failed: {0}")]
    LayerShell(String),

    #[error("Hyprland workspace query failed: {0}")]
    WorkspaceQuery(#[from] hyprland::shared::HyprError),

    #[error("Workspace channel setup failed: {0}")]
    WorkspaceChannel(String),

    #[error("Time formatting failed: {0}")]
    TimeFormat(String),

    #[error("Widget creation failed: {0}")]
    WidgetCreation(String),
}

pub type Result<T> = std::result::Result<T, AppError>;