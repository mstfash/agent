use anyhow::Result;
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};

use stakpak_api::ClientConfig;

pub mod local_tools;
pub mod remote_tools;
pub mod tool_container;

use tokio::{net::TcpListener, sync::broadcast::Receiver};
pub use tool_container::ToolContainer;
use tracing::error;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ToolMode {
    /// Only local tools (no API key required)
    LocalOnly,
    /// Only remote tools (requires API key)
    RemoteOnly,
    /// Both local and remote tools (requires API key)
    Combined,
}

impl std::fmt::Display for ToolMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ToolMode::LocalOnly => "local",
            ToolMode::RemoteOnly => "remote",
            ToolMode::Combined => "combined",
        };
        write!(f, "{}", s)
    }
}

impl std::str::FromStr for ToolMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "local" => Ok(ToolMode::LocalOnly),
            "remote" => Ok(ToolMode::RemoteOnly),
            "combined" => Ok(ToolMode::Combined),
            _ => Err(format!("Invalid tool mode: {}", s)),
        }
    }
}

pub struct MCPServerConfig {
    pub api: ClientConfig,
    pub bind_address: String,
    pub redact_secrets: bool,
    pub privacy_mode: bool,
    pub tool_mode: ToolMode,
}

pub struct MCPServerConfigWithoutBindAddress {
    pub api: ClientConfig,
    pub redact_secrets: bool,
    pub privacy_mode: bool,
    pub tool_mode: ToolMode,
}

/// Initialize gitleaks configuration if secret redaction is enabled
async fn init_gitleaks_if_needed(redact_secrets: bool, privacy_mode: bool) {
    if redact_secrets {
        tokio::spawn(async move {
            match std::panic::catch_unwind(|| {
                stakpak_shared::secrets::initialize_gitleaks_config(privacy_mode)
            }) {
                Ok(_rule_count) => {}
                Err(_) => {
                    // Failed to initialize, will initialize on first use
                }
            }
        });
    }
}

/// Create graceful shutdown handler
async fn create_shutdown_handler(shutdown_rx: Option<Receiver<()>>) {
    if let Some(mut shutdown_rx) = shutdown_rx {
        let _ = shutdown_rx.recv().await;
    } else {
        tracing::info!("Setting up signal handlers for graceful shutdown");

        // Handle both SIGINT (Ctrl+C) and SIGTERM
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(signal) => signal,
                Err(_) => {
                    // Fall back to basic ctrl_c handler
                    match tokio::signal::ctrl_c().await {
                        Ok(()) => {
                            return;
                        }
                        Err(_) => {
                            tokio::time::sleep(tokio::time::Duration::from_secs(u64::MAX)).await;
                            return;
                        }
                    }
                }
            };

            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(_) => {
                    // Continue with just SIGINT
                    let _ = sigint.recv().await;
                    return;
                }
            };

            tokio::select! {
                _ = sigint.recv() => {
                }
                _ = sigterm.recv() => {
                }
            }
        }

        #[cfg(not(unix))]
        {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    tracing::info!("Received Ctrl+C signal, shutting down gracefully");
                }
                Err(e) => {
                    tracing::error!("Failed to listen for Ctrl+C signal: {}", e);
                    // Fall back to waiting indefinitely if signal handling fails
                    tokio::time::sleep(tokio::time::Duration::from_secs(u64::MAX)).await;
                }
            }
        }
    }
}

/// Internal helper function that contains the common server initialization logic
async fn start_server_internal(
    api: ClientConfig,
    redact_secrets: bool,
    privacy_mode: bool,
    tool_mode: ToolMode,
    tcp_listener: TcpListener,
    shutdown_rx: Option<Receiver<()>>,
) -> Result<()> {
    init_gitleaks_if_needed(redact_secrets, privacy_mode).await;

    let tool_container = match tool_mode {
        ToolMode::LocalOnly => ToolContainer::new(
            None,
            redact_secrets,
            privacy_mode,
            ToolContainer::tool_router_local(),
        ),
        ToolMode::RemoteOnly => ToolContainer::new(
            Some(api),
            redact_secrets,
            privacy_mode,
            ToolContainer::tool_router_remote(),
        ),
        ToolMode::Combined => ToolContainer::new(
            Some(api),
            redact_secrets,
            privacy_mode,
            ToolContainer::tool_router_local() + ToolContainer::tool_router_remote(),
        ),
    }
    .map_err(|e| {
        error!("Failed to create tool container: {}", e);
        anyhow::anyhow!("Failed to create tool container: {}", e)
    })?;

    let service = StreamableHttpService::new(
        move || Ok(tool_container.to_owned()),
        LocalSessionManager::default().into(),
        Default::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    axum::serve(tcp_listener, router)
        .with_graceful_shutdown(create_shutdown_handler(shutdown_rx))
        .await?;

    Ok(())
}

/// npx @modelcontextprotocol/inspector cargo run mcp
pub async fn start_server(
    config: MCPServerConfig,
    shutdown_rx: Option<Receiver<()>>,
) -> Result<()> {
    let tcp_listener = TcpListener::bind(config.bind_address).await?;
    start_server_internal(
        config.api,
        config.redact_secrets,
        config.privacy_mode,
        config.tool_mode,
        tcp_listener,
        shutdown_rx,
    )
    .await
}

/// Start server with a pre-bound TcpListener to avoid port collision race conditions
pub async fn start_server_with_listener(
    config: MCPServerConfigWithoutBindAddress,
    tcp_listener: TcpListener,
    shutdown_rx: Option<Receiver<()>>,
) -> Result<()> {
    start_server_internal(
        config.api,
        config.redact_secrets,
        config.privacy_mode,
        config.tool_mode,
        tcp_listener,
        shutdown_rx,
    )
    .await
}

/// Start server with local tools only (no API key required)
pub async fn start_local_server(
    bind_address: String,
    redact_secrets: bool,
    privacy_mode: bool,
    shutdown_rx: Option<Receiver<()>>,
) -> Result<()> {
    start_server(
        MCPServerConfig {
            api: ClientConfig {
                api_key: None,
                api_endpoint: "".to_string(),
            },
            bind_address,
            redact_secrets,
            privacy_mode,
            tool_mode: ToolMode::LocalOnly,
        },
        shutdown_rx,
    )
    .await
}

/// Start server with remote tools only (requires API key)
pub async fn start_remote_server(
    api_config: ClientConfig,
    bind_address: String,
    redact_secrets: bool,
    privacy_mode: bool,
    shutdown_rx: Option<Receiver<()>>,
) -> Result<()> {
    start_server(
        MCPServerConfig {
            api: api_config,
            bind_address,
            redact_secrets,
            privacy_mode,
            tool_mode: ToolMode::RemoteOnly,
        },
        shutdown_rx,
    )
    .await
}

/// Start server with combined tools (requires API key)
pub async fn start_combined_server(
    api_config: ClientConfig,
    bind_address: String,
    redact_secrets: bool,
    privacy_mode: bool,
    shutdown_rx: Option<Receiver<()>>,
) -> Result<()> {
    start_server(
        MCPServerConfig {
            api: api_config,
            bind_address,
            redact_secrets,
            privacy_mode,
            tool_mode: ToolMode::Combined,
        },
        shutdown_rx,
    )
    .await
}
