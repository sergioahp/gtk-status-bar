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
    WorkspaceQuery(String),

    #[error("Workspace channel setup failed: {0}")]
    WorkspaceChannel(String),

    #[error("Title channel setup failed: {0}")]
    TitleChannel(String),

    #[error("Battery channel setup failed: {0}")]
    BatteryChannel(String),

    #[error("Time formatting failed: {0}")]
    TimeFormat(String),

    #[error("Widget creation failed: {0}")]
    WidgetCreation(String),

    #[error("zbus failed {0}")]
    Zbus(zbus::Error),

    #[error("zbus fdo error {0}")]
    ZbusFdo(zbus::fdo::Error),

    #[error("zbus names error {0}")]
    ZbusNames(zbus_names::Error),

    #[error("zbus variant error {0}")]
    ZbusVariant(zbus::zvariant::Error),

}

pub type Result<T> = std::result::Result<T, AppError>;

impl From<zbus::Error> for AppError {
    fn from(err: zbus::Error) -> Self {
        AppError::Zbus(err)
    }
}

impl From<zbus::fdo::Error> for AppError {
    fn from(err: zbus::fdo::Error) -> Self {
        AppError::Zbus(err.into())
    }
}

impl From<zbus_names::Error> for AppError {
    fn from(err: zbus_names::Error) -> Self {
        AppError::ZbusNames(err)
    }
}

impl From<zbus::zvariant::Error> for AppError {
    fn from(err: zbus::zvariant::Error) -> Self {
        AppError::ZbusVariant(err)
    }
}
