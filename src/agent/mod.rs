// In-process goose ACP agent wiring: uses goose's ACP server as a Rust
// library (GooseAgentConnection) instead of spawning a process. The `--spawn`
// flag falls back to running a `goose-acp` binary over stdio via the acp
// crate's AcpAgent, for crash isolation / debugging.

pub mod env_setup;

use agent_client_protocol::schema::v1::{
    CancelNotification, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, ReadTextFileRequest, RequestPermissionRequest,
    RequestPermissionResponse, SessionNotification, SessionUpdate, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{
    Agent as SacpAgent, Client as SacpClient, ConnectionTo, Responder,
};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Events forwarded from the ACP connection into the UI task.
pub enum AgentEvent {
    Update {
        session_id: String,
        update: SessionUpdate,
    },
    Permission {
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    },
    ReadTextFile {
        request: ReadTextFileRequest,
        responder: Responder<agent_client_protocol::schema::v1::ReadTextFileResponse>,
    },
    WriteTextFile {
        request: WriteTextFileRequest,
        responder: Responder<WriteTextFileResponse>,
    },
}

/// How to reach the agent.
#[derive(Debug, Clone)]
pub enum Transport {
    /// goose as a library, in-process (default).
    InProcess { builtins: Vec<String> },
    /// Spawn a command (e.g. a goose-acp binary) and speak ACP over stdio.
    Spawn { command: String },
}

pub type AgentCx = ConnectionTo<SacpAgent>;

/// Connect to the agent and run `app(cx)` inside the connection. The client
/// side (permission prompts, fs requests, session-update routing) is wired
/// here; events flow into `events_tx`.
pub async fn connect_and_run<F, Fut>(transport: Transport, events_tx: mpsc::UnboundedSender<AgentEvent>, app: F) -> Result<()>
where
    F: FnOnce(AgentCx) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
    let permission_tx = events_tx.clone();
    let read_tx = events_tx.clone();
    let write_tx = events_tx.clone();
    let update_tx = events_tx;

    let builder = SacpClient
        .builder()
        .name("goose-readline")
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let tx = update_tx.clone();
                async move {
                    let _ = tx.send(AgentEvent::Update {
                        session_id: notification.session_id.to_string(),
                        update: notification.update,
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            move |request: RequestPermissionRequest, responder: Responder<RequestPermissionResponse>, _cx| {
                let tx = permission_tx.clone();
                async move {
                    let _ = tx.send(AgentEvent::Permission { request, responder });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: ReadTextFileRequest, responder, _cx| {
                let tx = read_tx.clone();
                async move {
                    let _ = tx.send(AgentEvent::ReadTextFile { request, responder });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: WriteTextFileRequest, responder: Responder<WriteTextFileResponse>, _cx| {
                let tx = write_tx.clone();
                async move {
                    let _ = tx.send(AgentEvent::WriteTextFile { request, responder });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        );

    match transport {
        Transport::InProcess { builtins } => {
            let server = Arc::new(make_in_process_server(builtins));
            let connection = GooseAgentConnection::new(server);
            builder
                .connect_with(connection, move |cx: AgentCx| async move {
                    app(cx).await.map_err(acp_error)
                })
                .await
                .map_err(|e| anyhow!("agent connection failed: {e}"))
        }
        Transport::Spawn { command } => {
            let args = command
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let agent = agent_client_protocol::AcpAgent::from_args(args)
                .map_err(|e| anyhow!("failed to parse spawn command {command:?}: {e}"))?;
            builder
                .connect_with(agent, move |cx: AgentCx| async move {
                    app(cx).await.map_err(acp_error)
                })
                .await
                .map_err(|e| anyhow!("agent connection failed: {e}"))
        }
    }
}

fn acp_error(err: anyhow::Error) -> agent_client_protocol::Error {
    agent_client_protocol::util::internal_error(format!("{err:#}"))
}

fn make_in_process_server(builtins: Vec<String>) -> goose::acp::server_factory::AcpServer {
    use goose::acp::server::AcpBuiltinSelection;
    use goose::acp::server_factory::{AcpServer, AcpServerFactoryConfig};
    use goose::agents::GoosePlatform;

    AcpServer::new(AcpServerFactoryConfig {
        builtins: AcpBuiltinSelection::from_requested(builtins),
        config_dir: goose::config::paths::Paths::config_dir(),
        goose_platform: GoosePlatform::GooseCli,
        additional_source_roots: Vec::new(),
        session_cwd: None,
        enable_scheduler: false,
    })
}

use goose::acp::server::GooseAgentConnection;

/// Send the initialize request and return the agent capabilities (as raw
/// JSON) for capability gating.
pub async fn initialize(cx: &AgentCx) -> Result<serde_json::Value> {
    let resp = cx
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await
        .map_err(|e| anyhow!("initialize failed: {e}"))?;
    Ok(serde_json::to_value(&resp).unwrap_or_default())
}

pub async fn new_session(cx: &AgentCx, cwd: &std::path::Path) -> Result<serde_json::Value> {
    let resp = cx
        .send_request(NewSessionRequest::new(cwd))
        .block_task()
        .await
        .map_err(|e| anyhow!("session/new failed: {e}"))?;
    Ok(serde_json::to_value(&resp).unwrap_or_default())
}

pub async fn load_session(cx: &AgentCx, session_id: &str, cwd: &std::path::Path) -> Result<serde_json::Value> {
    let resp = cx
        .send_request(LoadSessionRequest::new(session_id.to_string(), cwd))
        .block_task()
        .await
        .map_err(|e| anyhow!("session/load failed: {e}"))?;
    Ok(serde_json::to_value(&resp).unwrap_or_default())
}

pub async fn list_sessions(cx: &AgentCx) -> Result<serde_json::Value> {
    let resp = cx
        .send_request(ListSessionsRequest::new())
        .block_task()
        .await
        .map_err(|e| anyhow!("session/list failed: {e}"))?;
    Ok(serde_json::to_value(&resp).unwrap_or_default())
}

pub async fn set_config_option(
    cx: &AgentCx,
    session_id: &str,
    config_id: &str,
    value: &str,
) -> Result<serde_json::Value> {
    use agent_client_protocol::schema::v1::SetSessionConfigOptionRequest;
    let req = SetSessionConfigOptionRequest::new(
        session_id.to_string(),
        config_id.to_string(),
        value,
    );
    let resp = cx
        .send_request(req)
        .block_task()
        .await
        .map_err(|e| anyhow!("session/set_config_option failed: {e}"))?;
    Ok(serde_json::to_value(&resp).unwrap_or_default())
}

/// Fire-and-forget session cancel notification.
pub fn cancel_session(cx: &AgentCx, session_id: &str) {
    let _ = cx.send_notification(CancelNotification::new(session_id.to_string()));
}

/// A pending prompt turn: the request future resolves with the stop reason.
pub fn send_prompt(
    cx: &AgentCx,
    session_id: &str,
    text: &str,
) -> agent_client_protocol::SentRequest<agent_client_protocol::schema::v1::PromptResponse> {
    use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
    cx.send_request(PromptRequest::new(
        session_id.to_string(),
        vec![ContentBlock::Text(TextContent::new(text))],
    ))
}
