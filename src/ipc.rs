use std::env;
use std::fs::Permissions;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

pub const SOCKET_ENV: &str = "GTK_STATUS_BAR_SOCKET";
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum IpcRequest {
    List,
    Activate { target: String },
    SecondaryActivate { target: String },
    ContextMenu { target: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IpcTrayItem {
    pub index: usize,
    pub key: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub items: Vec<IpcTrayItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn success(items: Vec<IpcTrayItem>) -> Self {
        Self {
            ok: true,
            items,
            error: None,
        }
    }

    pub fn error(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            items: Vec::new(),
            error: Some(error.into()),
        }
    }
}

#[derive(Debug)]
pub struct IpcUiRequest {
    pub request: IpcRequest,
    pub response: oneshot::Sender<IpcResponse>,
}

pub fn socket_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os(SOCKET_ENV) {
        if path.is_empty() {
            bail!("{SOCKET_ENV} is set but empty");
        }
        return Ok(PathBuf::from(path));
    }

    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set; set it or GTK_STATUS_BAR_SOCKET")?;
    Ok(PathBuf::from(runtime_dir)
        .join("gtk-status-bar")
        .join("tray.sock"))
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                warn!(path = %self.0.display(), %error, "Could not remove tray IPC socket");
            }
        }
    }
}

async fn remove_stale_socket(path: &Path) -> Result<()> {
    match UnixStream::connect(path).await {
        Ok(_) => bail!(
            "another tray IPC server is already listening at {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("remove stale tray IPC socket {}", path.display())),
        Err(error) => {
            Err(error).with_context(|| format!("probe existing tray IPC socket {}", path.display()))
        }
    }
}

async fn bind_socket(path: &Path) -> Result<UnixListener> {
    let parent = path
        .parent()
        .with_context(|| format!("tray IPC socket path has no parent: {}", path.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("create tray IPC directory {}", parent.display()))?;
    tokio::fs::set_permissions(parent, Permissions::from_mode(0o700))
        .await
        .with_context(|| format!("secure tray IPC directory {}", parent.display()))?;
    remove_stale_socket(path).await?;

    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind tray IPC socket {}", path.display()))?;
    tokio::fs::set_permissions(path, Permissions::from_mode(0o600))
        .await
        .with_context(|| format!("secure tray IPC socket {}", path.display()))?;
    Ok(listener)
}

async fn handle_client(stream: UnixStream, ui_tx: mpsc::UnboundedSender<IpcUiRequest>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) => {
                debug!(%error, "Tray IPC client read failed");
                return;
            }
        };
        let request = match serde_json::from_str::<IpcRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = IpcResponse::error(format!("invalid request: {error}"));
                if write_response(&mut writer, &response).await.is_err() {
                    return;
                }
                continue;
            }
        };

        let (response_tx, response_rx) = oneshot::channel();
        if ui_tx
            .send(IpcUiRequest {
                request,
                response: response_tx,
            })
            .is_err()
        {
            let response = IpcResponse::error("tray UI is not available");
            let _ = write_response(&mut writer, &response).await;
            return;
        }
        let response = match tokio::time::timeout(RESPONSE_TIMEOUT, response_rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => IpcResponse::error("tray UI dropped the request"),
            Err(_) => IpcResponse::error("tray UI did not respond within 5 seconds"),
        };
        if write_response(&mut writer, &response).await.is_err() {
            return;
        }
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &IpcResponse,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(response).context("encode tray IPC response")?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .context("write tray IPC response")
}

pub async fn run_server(ui_tx: mpsc::UnboundedSender<IpcUiRequest>) -> Result<()> {
    let path = socket_path()?;
    let listener = bind_socket(&path).await?;
    let _cleanup = SocketCleanup(path.clone());
    info!(path = %path.display(), "Tray IPC server is listening");

    loop {
        let (stream, _) = listener.accept().await.context("accept tray IPC client")?;
        tokio::spawn(handle_client(stream, ui_tx.clone()));
    }
}

pub async fn send_request(request: &IpcRequest) -> Result<IpcResponse> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connect to tray IPC socket {}", path.display()))?;
    let (reader, mut writer) = stream.into_split();
    let mut encoded = serde_json::to_vec(request).context("encode tray IPC request")?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .context("write tray IPC request")?;

    let mut response = String::new();
    BufReader::new(reader)
        .read_line(&mut response)
        .await
        .context("read tray IPC response")?;
    if response.is_empty() {
        bail!("tray IPC server closed the connection without a response");
    }
    serde_json::from_str(&response).context("decode tray IPC response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips() {
        let request = IpcRequest::SecondaryActivate {
            target: "1".to_string(),
        };
        let encoded = serde_json::to_string(&request).expect("request should encode");
        assert_eq!(encoded, r#"{"command":"secondary-activate","target":"1"}"#);
        assert_eq!(
            serde_json::from_str::<IpcRequest>(&encoded).expect("request should decode"),
            request
        );
    }
}
