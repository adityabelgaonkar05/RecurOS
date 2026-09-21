//! A small, self-hostable MCP relay and the persistent outbound connection
//! used by a local `ctx` node.
//!
//! The relay deliberately stores routing and identity metadata only. Claims,
//! documents and packs never cross its persistence boundary: a raw JSON-RPC
//! request is forwarded to the local node, which runs the existing MCP
//! handler against the user's local RecurOS store.

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use axum::extract::ws::{Message as AxumMessage, WebSocket};
use axum::extract::{Path as AxumPath, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use mongodb::bson::{Document, doc};
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, Database, IndexModel};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, RwLock, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as ClientMessage;
use ulid::Ulid;

use ctx_app::App;
use ctx_core::BranchRef;
use ctx_git::CtxHome;

pub const DEFAULT_RELAY_ADDR: &str = "127.0.0.1:8788";
const NODE_DIR: &str = ".node";
const NODE_CONFIG: &str = "relay.json";
const NODE_KEY: &str = "device.ed25519";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Values printed exactly once by `ctx relay init`.
#[derive(Debug, Clone)]
pub struct RelayInit {
    pub bootstrap_code: String,
    pub connector_secret: String,
}

/// A relay's non-sensitive view of an enrolled device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub active: bool,
    pub revoked: bool,
}

/// Selects where the relay keeps its small, non-context metadata database.
///
/// A configured MongoDB URL takes precedence. Without one, the normal
/// self-hosted path remains the SQLite file under `--data`; in-memory storage
/// exists for tests and is deliberately not exposed as a production CLI mode.
#[derive(Debug, Clone, Default)]
pub struct RelayStorage {
    pub mongodb_url: Option<String>,
    pub mongodb_database: Option<String>,
}

impl RelayStorage {
    /// Resolve command-line values first, then the conventional deployment
    /// environment variables. Empty strings behave as unset values, which
    /// makes `MONGODB_URL=` safe in Docker Compose files.
    pub fn from_environment(mongodb_url: Option<String>, mongodb_database: Option<String>) -> Self {
        RelayStorage {
            mongodb_url: clean_setting(mongodb_url)
                .or_else(|| std::env::var("MONGODB_URL").ok().and_then(clean_string)),
            mongodb_database: clean_setting(mongodb_database).or_else(|| {
                std::env::var("MONGODB_DATABASE")
                    .ok()
                    .and_then(clean_string)
            }),
        }
    }

    fn database_name(&self) -> String {
        self.mongodb_database
            .clone()
            .unwrap_or_else(|| "recuros_relay".to_owned())
    }
}

fn clean_setting(value: Option<String>) -> Option<String> {
    value.and_then(clean_string)
}

fn clean_string(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Initialise a new relay data directory. The bootstrap code enrols exactly
/// one first device; additional account/device management can be layered on
/// top without changing the node wire protocol.
pub fn init(data_dir: &Path) -> Result<RelayInit> {
    init_with_storage(data_dir, RelayStorage::default())
}

/// Initialise a relay using SQLite by default or MongoDB when a URL is set.
pub fn init_with_storage(data_dir: &Path, storage: RelayStorage) -> Result<RelayInit> {
    block_on(async {
        let db = open_store(data_dir, &storage).await?;
        initialise_store(db.as_ref(), data_dir).await
    })
}

/// Create a revocable connector credential. It authenticates an MCP caller,
/// never a node or an administrator.
pub fn create_connector_secret(data_dir: &Path, label: &str) -> Result<String> {
    create_connector_secret_with_storage(data_dir, label, RelayStorage::default())
}

pub fn create_connector_secret_with_storage(
    data_dir: &Path,
    label: &str,
    storage: RelayStorage,
) -> Result<String> {
    block_on(async {
        let db = open_store(data_dir, &storage).await?;
        db.require_initialised().await?;
        let token = secret("rcm_")?;
        db.insert_connector(&token, "owner", label).await?;
        Ok(token)
    })
}

/// Revoke a device key. Existing connections are checked before every relayed
/// request, so revocation takes effect without waiting for a reconnect.
pub fn revoke_device(data_dir: &Path, device_id: &str) -> Result<()> {
    revoke_device_with_storage(data_dir, device_id, RelayStorage::default())
}

pub fn revoke_device_with_storage(
    data_dir: &Path,
    device_id: &str,
    storage: RelayStorage,
) -> Result<()> {
    block_on(async {
        let db = open_store(data_dir, &storage).await?;
        db.require_initialised().await?;
        if !db.revoke_device(device_id).await? {
            bail!("no active device named `{device_id}`")
        }
        Ok(())
    })
}

/// List enrolled devices without exposing their public keys.
pub fn devices(data_dir: &Path) -> Result<Vec<DeviceInfo>> {
    devices_with_storage(data_dir, RelayStorage::default())
}

pub fn devices_with_storage(data_dir: &Path, storage: RelayStorage) -> Result<Vec<DeviceInfo>> {
    block_on(async {
        let db = open_store(data_dir, &storage).await?;
        db.require_initialised().await?;
        db.devices().await
    })
}

/// Serve the public relay. TLS belongs at the public reverse proxy; the node
/// chooses `wss://` automatically when the configured relay URL is `https://`.
pub fn serve(data_dir: PathBuf, addr: &str) -> Result<()> {
    serve_with_storage(data_dir, addr, RelayStorage::default())
}

/// Serve a relay with the selected metadata store. This remains a control
/// plane only: no context records are inserted into either backend.
pub fn serve_with_storage(data_dir: PathBuf, addr: &str, storage: RelayStorage) -> Result<()> {
    let addr = addr.to_owned();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(async move {
        let db = open_store(&data_dir, &storage).await?;
        db.require_initialised().await?;
        let state = RelayState::new(db);
        let router = router(state);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("binding relay at {addr}"))?;
        eprintln!("ctx relay listening on http://{addr}. Put TLS in front of it before exposing it publicly.");
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
        .await?;
        Ok(())
    });
    rt.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Pair this local node with a relay. The bootstrap code is consumed by the
/// relay and is not kept locally; subsequent authentication is public-key
/// challenge/response only.
pub fn node_login(
    home: &CtxHome,
    relay_url: &str,
    bootstrap_code: &str,
    branch: &BranchRef,
) -> Result<String> {
    ensure_crypto_provider()?;
    let key = load_or_create_key(home)?;
    let relay_url = normalise_relay_url(relay_url)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let device_id = rt.block_on(async {
        let (mut ws, _) = connect_async(node_ws_url(&relay_url)?).await?;
        send_client_wire(
            &mut ws,
            &Wire::Enroll {
                bootstrap_code: bootstrap_code.to_owned(),
                public_key: encode_public_key(&key.verifying_key()),
            },
        )
        .await?;
        authenticate_node(&mut ws, &key).await
    })?;
    save_node_config(
        home,
        &NodeConfig {
            relay_url,
            device_id: device_id.clone(),
            branch: branch.to_string(),
        },
    )?;
    Ok(device_id)
}

/// Keep the local context node reachable by remote MCP clients. It owns the
/// execution path; the relay only carries request/response envelopes.
pub fn node_start(
    home: CtxHome,
    relay_override: Option<&str>,
    branch_override: Option<&str>,
) -> Result<()> {
    ensure_crypto_provider()?;
    let mut config = load_node_config(&home)?.context(
        "node is not paired yet; run `ctx node login --relay URL --code BOOTSTRAP_CODE`",
    )?;
    if let Some(url) = relay_override {
        config.relay_url = normalise_relay_url(url)?;
    }
    if let Some(branch) = branch_override {
        config.branch = BranchRef::new(branch)?.to_string();
    }
    if relay_override.is_some() || branch_override.is_some() {
        save_node_config(&home, &config)?;
    }
    let key = load_or_create_key(&home)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_node(home, config, key))
}

/// Report only non-sensitive node configuration for `ctx status` and users.
pub fn node_status(home: &CtxHome) -> Result<Option<(String, String, String)>> {
    Ok(load_node_config(home)?.map(|c| (c.relay_url, c.device_id, c.branch)))
}

#[derive(Clone)]
struct RelayState {
    db: Arc<dyn RelayStore>,
    nodes: Arc<RwLock<HashMap<String, NodeConnection>>>,
}

impl RelayState {
    fn new(db: Arc<dyn RelayStore>) -> Self {
        RelayState {
            db,
            nodes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn install_node(&self, user: String, node: NodeConnection) -> Option<NodeConnection> {
        self.nodes.write().await.insert(user, node)
    }

    async fn remove_node(&self, user: &str, connection_id: &str) {
        let mut nodes = self.nodes.write().await;
        if nodes
            .get(user)
            .is_some_and(|node| node.connection_id == connection_id)
        {
            nodes.remove(user);
        }
    }

    async fn forward(&self, user: &str, payload: Value) -> Result<Value, RelayError> {
        let node = self
            .nodes
            .read()
            .await
            .get(user)
            .cloned()
            .ok_or(RelayError::NodeOffline)?;
        if self
            .db
            .device_user(&node.device_id)
            .await
            .ok()
            .flatten()
            .as_deref()
            != Some(user)
        {
            return Err(RelayError::NodeOffline);
        }
        let request_id = Ulid::new().to_string();
        let (tx, rx) = oneshot::channel();
        node.pending.lock().await.insert(request_id.clone(), tx);
        let message = Wire::Request {
            request_id: request_id.clone(),
            payload,
        };
        let message = text_message(&message).map_err(|_| RelayError::NodeOffline)?;
        if node.tx.send(message).await.is_err() {
            node.pending.lock().await.remove(&request_id);
            return Err(RelayError::NodeOffline);
        }
        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(RelayError::NodeOffline),
            Err(_) => {
                node.pending.lock().await.remove(&request_id);
                Err(RelayError::TimedOut)
            }
        }
    }
}

#[derive(Clone)]
struct NodeConnection {
    connection_id: String,
    device_id: String,
    tx: mpsc::Sender<AxumMessage>,
    pending: Arc<AsyncMutex<HashMap<String, oneshot::Sender<Value>>>>,
}

#[derive(Debug)]
enum RelayError {
    NodeOffline,
    TimedOut,
}

impl RelayError {
    fn message(&self) -> &'static str {
        match self {
            RelayError::NodeOffline => {
                "The RecurOS node for this account is offline. Start `ctx node start` on the paired device."
            }
            RelayError::TimedOut => "The RecurOS node did not answer before the relay timeout.",
        }
    }
}

fn router(state: RelayState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/node/connect", get(node_connect))
        .route("/mcp", post(mcp))
        // A path secret is retained for clients that cannot set headers. New
        // connector configurations should use `Authorization: Bearer …` on
        // the stable `/mcp` endpoint so credentials do not appear in URLs.
        .route("/mcp/{secret}", post(mcp_with_path_secret))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "service": "recuros-relay", "version": ctx_app::VERSION}))
}

async fn node_connect(ws: WebSocketUpgrade, State(state): State<RelayState>) -> Response {
    ws.on_upgrade(move |socket| serve_node_socket(socket, state))
}

async fn mcp(
    headers: HeaderMap,
    State(state): State<RelayState>,
    Json(payload): Json<Value>,
) -> Response {
    let secret = bearer_secret(&headers);
    forward_mcp(state, secret, payload).await
}

async fn mcp_with_path_secret(
    AxumPath(secret): AxumPath<String>,
    State(state): State<RelayState>,
    Json(payload): Json<Value>,
) -> Response {
    forward_mcp(state, Some(secret.as_str()), payload).await
}

async fn forward_mcp(state: RelayState, secret: Option<&str>, payload: Value) -> Response {
    let Some(secret) = secret else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rpc_error(
                &payload,
                -32001,
                "Missing or invalid connector credential.",
            )),
        )
            .into_response();
    };
    let Some(user) = state.db.connector_user(secret).await.unwrap_or(None) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(rpc_error(
                &payload,
                -32001,
                "Invalid or revoked connector credential.",
            )),
        )
            .into_response();
    };
    match state.forward(&user, payload.clone()).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(rpc_error(&payload, -32002, error.message())),
        )
            .into_response(),
    }
}

fn bearer_secret(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty())
}

fn rpc_error(payload: &Value, code: i64, message: &str) -> Value {
    if let Some(batch) = payload.as_array() {
        return Value::Array(
            batch
                .iter()
                .filter(|item| item.get("id").is_some())
                .map(|item| rpc_error(item, code, message))
                .collect(),
        );
    }
    json!({
        "jsonrpc": "2.0",
        "id": payload.get("id").cloned().unwrap_or(Value::Null),
        "error": {"code": code, "message": message}
    })
}

async fn serve_node_socket(mut socket: WebSocket, state: RelayState) {
    let outcome = async {
        let first = recv_wire(&mut socket).await?;
        let (device_id, public_key) = match first {
            Wire::Enroll {
                bootstrap_code,
                public_key,
            } => state.db.enrol(&bootstrap_code, &public_key).await?,
            Wire::Hello { device_id } => {
                let key = state
                    .db
                    .device_key(&device_id)
                    .await?
                    .context("unknown or revoked device")?;
                (device_id, key)
            }
            _ => bail!("expected node hello or enrolment"),
        };
        let nonce = random_bytes(32)?;
        send_wire(
            &mut socket,
            &Wire::Challenge {
                device_id: device_id.clone(),
                nonce: URL_SAFE_NO_PAD.encode(&nonce),
            },
        )
        .await?;
        let Wire::Authenticate { signature } = recv_wire(&mut socket).await? else {
            bail!("expected node authentication")
        };
        verify_signature(&public_key, &device_id, &nonce, &signature)?;
        let user = state
            .db
            .device_user(&device_id)
            .await?
            .context("unknown or revoked device")?;
        state.db.set_active(&device_id).await?;
        send_wire(
            &mut socket,
            &Wire::Ready {
                device_id: device_id.clone(),
            },
        )
        .await?;

        let (mut writer, mut reader) = socket.split();
        let (tx, mut rx) = mpsc::channel::<AxumMessage>(32);
        let connection = NodeConnection {
            connection_id: Ulid::new().to_string(),
            device_id: device_id.clone(),
            tx,
            pending: Arc::new(AsyncMutex::new(HashMap::new())),
        };
        let connection_id = connection.connection_id.clone();
        let previous = state.install_node(user.clone(), connection.clone()).await;
        if let Some(previous) = previous {
            fail_pending(
                &previous,
                json!({"error": "replaced by a newer node connection"}),
            )
            .await;
        }
        let write_task = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if writer.send(message).await.is_err() {
                    break;
                }
            }
        });
        while let Some(frame) = reader.next().await {
            let frame = frame?;
            let AxumMessage::Text(text) = frame else {
                continue;
            };
            let Wire::Response {
                request_id,
                payload,
            } = serde_json::from_str::<Wire>(&text)?
            else {
                continue;
            };
            if let Some(waiter) = connection.pending.lock().await.remove(&request_id) {
                let _ = waiter.send(payload);
            }
        }
        write_task.abort();
        state.remove_node(&user, &connection_id).await;
        fail_pending(&connection, json!({"error": "node disconnected"})).await;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = outcome {
        eprintln!("ctx relay: rejected node connection: {error:#}");
    }
}

async fn fail_pending(connection: &NodeConnection, payload: Value) {
    let pending = std::mem::take(&mut *connection.pending.lock().await);
    for (_, waiter) in pending {
        let _ = waiter.send(payload.clone());
    }
}

async fn run_node(home: CtxHome, config: NodeConfig, key: SigningKey) -> Result<()> {
    let mut app = App::open(home, None)?;
    let branch = BranchRef::new(&config.branch)?;
    app.set_active(&branch)?;
    loop {
        match run_node_connection(&mut app, &config, &key).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("ctx node: relay connection ended ({error:#}); retrying in 3 seconds");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn run_node_connection(app: &mut App, config: &NodeConfig, key: &SigningKey) -> Result<()> {
    let (mut ws, _) = connect_async(node_ws_url(&config.relay_url)?).await?;
    send_client_wire(
        &mut ws,
        &Wire::Hello {
            device_id: config.device_id.clone(),
        },
    )
    .await?;
    let authenticated = authenticate_node(&mut ws, key).await?;
    if authenticated != config.device_id {
        bail!("relay authenticated a different device")
    }
    eprintln!(
        "ctx node: connected to {} as {}",
        config.relay_url, config.device_id
    );
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("ctx node: stopping");
                return Ok(());
            }
            frame = ws.next() => {
                let Some(frame) = frame else { bail!("relay closed the connection") };
                let frame = frame?;
                let ClientMessage::Text(text) = frame else { continue };
                let Wire::Request { request_id, payload } = serde_json::from_str::<Wire>(&text)? else { continue };
                let response = execute_mcp(app, payload);
                send_client_wire(&mut ws, &Wire::Response { request_id, payload: response }).await?;
            }
        }
    }
}

async fn authenticate_node<S>(socket: &mut S, key: &SigningKey) -> Result<String>
where
    S: futures_util::Sink<ClientMessage, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let Wire::Challenge { device_id, nonce } = recv_client_wire(socket).await? else {
        bail!("relay did not send a challenge")
    };
    let nonce = URL_SAFE_NO_PAD.decode(nonce.as_bytes())?;
    let signature = key.sign(&auth_bytes(&device_id, &nonce));
    send_client_wire(
        socket,
        &Wire::Authenticate {
            signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        },
    )
    .await?;
    let Wire::Ready { device_id: ready } = recv_client_wire(socket).await? else {
        bail!("relay did not accept the device")
    };
    if ready != device_id {
        bail!("relay changed the challenged device id")
    }
    Ok(ready)
}

fn execute_mcp(app: &mut App, payload: Value) -> Value {
    let mut wrote = false;
    match payload {
        Value::Array(messages) => Value::Array(
            messages
                .iter()
                .filter_map(|message| ctx_mcp::handle(app, message, &mut wrote))
                .collect(),
        ),
        message => ctx_mcp::handle(app, &message, &mut wrote).unwrap_or_else(|| json!({})),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Wire {
    Enroll {
        bootstrap_code: String,
        public_key: String,
    },
    Hello {
        device_id: String,
    },
    Challenge {
        device_id: String,
        nonce: String,
    },
    Authenticate {
        signature: String,
    },
    Ready {
        device_id: String,
    },
    Request {
        request_id: String,
        payload: Value,
    },
    Response {
        request_id: String,
        payload: Value,
    },
}

async fn recv_wire(socket: &mut WebSocket) -> Result<Wire> {
    let Some(frame) = socket.recv().await else {
        bail!("connection closed")
    };
    let AxumMessage::Text(text) = frame? else {
        bail!("expected text websocket frame")
    };
    Ok(serde_json::from_str(&text)?)
}

async fn send_wire(socket: &mut WebSocket, wire: &Wire) -> Result<()> {
    socket.send(text_message(wire)?).await?;
    Ok(())
}

async fn recv_client_wire<S>(socket: &mut S) -> Result<Wire>
where
    S: futures_util::Stream<Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let Some(frame) = socket.next().await else {
        bail!("connection closed")
    };
    let ClientMessage::Text(text) = frame? else {
        bail!("expected text websocket frame")
    };
    Ok(serde_json::from_str(&text)?)
}

async fn send_client_wire<S>(socket: &mut S, wire: &Wire) -> Result<()>
where
    S: futures_util::Sink<ClientMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    socket
        .send(ClientMessage::Text(serde_json::to_string(wire)?.into()))
        .await?;
    Ok(())
}

fn text_message(wire: &Wire) -> Result<AxumMessage> {
    Ok(AxumMessage::Text(serde_json::to_string(wire)?.into()))
}

fn verify_signature(
    public_key: &str,
    device_id: &str,
    nonce: &[u8],
    signature: &str,
) -> Result<()> {
    let key = decode_public_key(public_key)?;
    let signature = URL_SAFE_NO_PAD.decode(signature.as_bytes())?;
    let signature = Signature::from_slice(&signature)?;
    key.verify(&auth_bytes(device_id, nonce), &signature)
        .map_err(|_| anyhow!("invalid device signature"))
}

fn auth_bytes(device_id: &str, nonce: &[u8]) -> Vec<u8> {
    let mut out = b"recuros-node-auth-v1\0".to_vec();
    out.extend_from_slice(device_id.as_bytes());
    out.push(0);
    out.extend_from_slice(nonce);
    out
}

fn encode_public_key(key: &VerifyingKey) -> String {
    URL_SAFE_NO_PAD.encode(key.to_bytes())
}

fn decode_public_key(value: &str) -> Result<VerifyingKey> {
    let bytes = URL_SAFE_NO_PAD.decode(value.as_bytes())?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("device public key has the wrong length"))?;
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeConfig {
    relay_url: String,
    device_id: String,
    #[serde(default = "default_node_branch")]
    branch: String,
}

fn default_node_branch() -> String {
    BranchRef::DEFAULT.to_owned()
}

fn node_dir(home: &CtxHome) -> PathBuf {
    home.root().join(NODE_DIR)
}

fn node_config_path(home: &CtxHome) -> PathBuf {
    node_dir(home).join(NODE_CONFIG)
}

fn key_path(home: &CtxHome) -> PathBuf {
    node_dir(home).join(NODE_KEY)
}

fn load_node_config(home: &CtxHome) -> Result<Option<NodeConfig>> {
    let path = node_config_path(home);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn save_node_config(home: &CtxHome, config: &NodeConfig) -> Result<()> {
    let path = node_config_path(home);
    write_private(&path, &serde_json::to_string_pretty(config)?)
}

fn load_or_create_key(home: &CtxHome) -> Result<SigningKey> {
    let path = key_path(home);
    match fs::read(&path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow!("{} has the wrong length", path.display()))?;
            Ok(SigningKey::from_bytes(&bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let bytes = random_bytes(32)?;
            let bytes: [u8; 32] = bytes.try_into().expect("32 bytes requested");
            write_private(&path, bytes)?;
            Ok(SigningKey::from_bytes(&bytes))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_ref())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    Ok(())
}

fn normalise_relay_url(raw: &str) -> Result<String> {
    let url = raw.trim().trim_end_matches('/');
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("relay URL must start with http:// or https://")
    }
    Ok(url.to_owned())
}

fn node_ws_url(relay_url: &str) -> Result<String> {
    let ws = relay_url
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            relay_url
                .strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .context("relay URL must start with http:// or https://")?;
    Ok(format!("{ws}/v1/node/connect"))
}

/// `rustls` 0.23 deliberately makes the cryptographic implementation an
/// application choice. Explicitly selecting ring keeps `wss://` node
/// connections working even when another dependency changes Rustls defaults.
fn ensure_crypto_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow!("could not install the Rustls crypto provider"))?;
    }
    Ok(())
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("OS randomness failed: {error}"))?;
    Ok(bytes)
}

fn secret(prefix: &str) -> Result<String> {
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(random_bytes(32)?)
    ))
}

fn hash_secret(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

#[async_trait]
trait RelayStore: Send + Sync {
    async fn create_schema(&self) -> Result<()>;
    async fn require_initialised(&self) -> Result<()>;
    async fn meta(&self, key: &str) -> Result<Option<String>>;
    async fn set_meta(&self, key: &str, value: &str) -> Result<()>;
    async fn insert_connector(&self, secret: &str, user: &str, label: &str) -> Result<()>;
    async fn connector_user(&self, secret: &str) -> Result<Option<String>>;
    async fn enrol(&self, code: &str, public_key: &str) -> Result<(String, String)>;
    async fn device_key(&self, device_id: &str) -> Result<Option<String>>;
    async fn device_user(&self, device_id: &str) -> Result<Option<String>>;
    async fn set_active(&self, device_id: &str) -> Result<()>;
    async fn revoke_device(&self, device_id: &str) -> Result<bool>;
    async fn devices(&self) -> Result<Vec<DeviceInfo>>;
}

async fn open_store(data_dir: &Path, storage: &RelayStorage) -> Result<Arc<dyn RelayStore>> {
    if let Some(url) = storage.mongodb_url.as_deref() {
        return Ok(Arc::new(
            MongoRelayStore::connect(url, &storage.database_name()).await?,
        ));
    }
    Ok(Arc::new(SqliteRelayStore::new(data_dir)))
}

fn block_on<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(future);
    // The MongoDB driver owns background monitoring tasks. All relay writes
    // above have completed before this point, so do not let those idle tasks
    // keep one-shot CLI commands such as `ctx relay token` alive forever.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn initialise_store(store: &dyn RelayStore, data_dir: &Path) -> Result<RelayInit> {
    store.create_schema().await?;
    if store.meta("bootstrap_hash").await?.is_some() {
        bail!(
            "{} is already initialised; create another connector secret with `ctx relay token create`",
            data_dir.display()
        );
    }
    let bootstrap_code = secret("rcb_")?;
    let connector_secret = secret("rcm_")?;
    store
        .set_meta("bootstrap_hash", &hash_secret(&bootstrap_code))
        .await?;
    store.set_meta("bootstrap_used", "false").await?;
    store
        .insert_connector(&connector_secret, "owner", "initial connector")
        .await?;
    Ok(RelayInit {
        bootstrap_code,
        connector_secret,
    })
}

#[derive(Clone)]
struct SqliteRelayStore {
    path: PathBuf,
}

impl SqliteRelayStore {
    fn new(data_dir: &Path) -> Self {
        SqliteRelayStore {
            path: data_dir.join("relay.db"),
        }
    }

    fn connection(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Connection::open(&self.path)?)
    }
}

#[async_trait]
impl RelayStore for SqliteRelayStore {
    async fn create_schema(&self) -> Result<()> {
        self.connection()?.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS devices (
              id TEXT PRIMARY KEY, user_id TEXT NOT NULL, public_key TEXT NOT NULL UNIQUE,
              revoked INTEGER NOT NULL DEFAULT 0, active INTEGER NOT NULL DEFAULT 0,
              created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS connector_tokens (
              token_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL, label TEXT NOT NULL,
              revoked INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS devices_active ON devices(user_id, active);
            ",
        )?;
        Ok(())
    }

    async fn require_initialised(&self) -> Result<()> {
        self.create_schema().await?;
        if self.meta("bootstrap_hash").await?.is_none() {
            bail!(
                "relay is not initialised; run `ctx relay init --data {}` first",
                self.path.parent().unwrap_or(Path::new(".")).display()
            )
        }
        Ok(())
    }

    async fn meta(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .connection()?
            .query_row("SELECT value FROM meta WHERE key = ?", [key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value)
    }

    async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    async fn insert_connector(&self, secret: &str, user: &str, label: &str) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO connector_tokens(token_hash, user_id, label, created_at) VALUES (?, ?, ?, ?)",
            params![hash_secret(secret), user, label, Ulid::new().to_string()],
        )?;
        Ok(())
    }

    async fn connector_user(&self, secret: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT user_id FROM connector_tokens WHERE token_hash = ? AND revoked = 0",
                [hash_secret(secret)],
                |row| row.get(0),
            )
            .optional()?)
    }

    async fn enrol(&self, code: &str, public_key: &str) -> Result<(String, String)> {
        self.require_initialised().await?;
        let public_key = decode_public_key(public_key).map(|key| encode_public_key(&key))?;
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let expected: String = tx.query_row(
            "SELECT value FROM meta WHERE key = 'bootstrap_hash'",
            [],
            |row| row.get(0),
        )?;
        let used: String = tx.query_row(
            "SELECT value FROM meta WHERE key = 'bootstrap_used'",
            [],
            |row| row.get(0),
        )?;
        if used == "true" || !constant_time_eq(expected.as_bytes(), hash_secret(code).as_bytes()) {
            bail!("bootstrap code is invalid or has already been used")
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM devices WHERE public_key = ? AND revoked = 0",
                [&public_key],
                |row| row.get(0),
            )
            .optional()?;
        let is_new = existing.is_none();
        let device_id = existing.unwrap_or_else(|| Ulid::new().to_string());
        if is_new {
            tx.execute(
                "INSERT INTO devices(id, user_id, public_key, created_at) VALUES (?, 'owner', ?, ?)",
                params![device_id, public_key, Ulid::new().to_string()],
            )?;
        }
        tx.execute(
            "UPDATE meta SET value = 'true' WHERE key = 'bootstrap_used'",
            [],
        )?;
        tx.commit()?;
        Ok((device_id, public_key))
    }

    async fn device_key(&self, device_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT public_key FROM devices WHERE id = ? AND revoked = 0",
                [device_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    async fn device_user(&self, device_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT user_id FROM devices WHERE id = ? AND revoked = 0",
                [device_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    async fn set_active(&self, device_id: &str) -> Result<()> {
        let user: String = self.connection()?.query_row(
            "SELECT user_id FROM devices WHERE id = ? AND revoked = 0",
            [device_id],
            |row| row.get(0),
        )?;
        let conn = self.connection()?;
        conn.execute("UPDATE devices SET active = 0 WHERE user_id = ?", [user])?;
        conn.execute("UPDATE devices SET active = 1 WHERE id = ?", [device_id])?;
        Ok(())
    }

    async fn revoke_device(&self, device_id: &str) -> Result<bool> {
        let changed = self.connection()?.execute(
            "UPDATE devices SET revoked = 1, active = 0 WHERE id = ? AND revoked = 0",
            [device_id],
        )?;
        Ok(changed > 0)
    }

    async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let conn = self.connection()?;
        let mut statement = conn.prepare(
            "SELECT id, active, revoked FROM devices WHERE user_id = 'owner' ORDER BY created_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(DeviceInfo {
                id: row.get(0)?,
                active: row.get::<_, i64>(1)? != 0,
                revoked: row.get::<_, i64>(2)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct MemoryRelayStore {
    state: Arc<StdMutex<MemoryRelayData>>,
}

#[cfg(test)]
#[derive(Default)]
struct MemoryRelayData {
    meta: HashMap<String, String>,
    devices: HashMap<String, MemoryDevice>,
    connector_tokens: HashMap<String, MemoryConnector>,
}

#[cfg(test)]
struct MemoryDevice {
    user_id: String,
    public_key: String,
    revoked: bool,
    active: bool,
    created_at: String,
}

#[cfg(test)]
struct MemoryConnector {
    user_id: String,
    revoked: bool,
}

#[cfg(test)]
impl MemoryRelayStore {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryRelayData>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("relay memory store lock was poisoned"))
    }
}

#[cfg(test)]
#[async_trait]
impl RelayStore for MemoryRelayStore {
    async fn create_schema(&self) -> Result<()> {
        Ok(())
    }

    async fn require_initialised(&self) -> Result<()> {
        if self.meta("bootstrap_hash").await?.is_none() {
            bail!("relay is not initialised; initialise the in-memory store first")
        }
        Ok(())
    }

    async fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.lock()?.meta.get(key).cloned())
    }

    async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.lock()?.meta.insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    async fn insert_connector(&self, secret: &str, user: &str, _label: &str) -> Result<()> {
        self.lock()?.connector_tokens.insert(
            hash_secret(secret),
            MemoryConnector {
                user_id: user.to_owned(),
                revoked: false,
            },
        );
        Ok(())
    }

    async fn connector_user(&self, secret: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .connector_tokens
            .get(&hash_secret(secret))
            .filter(|token| !token.revoked)
            .map(|token| token.user_id.clone()))
    }

    async fn enrol(&self, code: &str, public_key: &str) -> Result<(String, String)> {
        self.require_initialised().await?;
        let public_key = decode_public_key(public_key).map(|key| encode_public_key(&key))?;
        let mut state = self.lock()?;
        let expected = state
            .meta
            .get("bootstrap_hash")
            .context("relay bootstrap state is missing")?;
        let used = state
            .meta
            .get("bootstrap_used")
            .is_some_and(|value| value == "true");
        if used || !constant_time_eq(expected.as_bytes(), hash_secret(code).as_bytes()) {
            bail!("bootstrap code is invalid or has already been used")
        }
        let device_id = Ulid::new().to_string();
        state.devices.insert(
            device_id.clone(),
            MemoryDevice {
                user_id: "owner".to_owned(),
                public_key: public_key.clone(),
                revoked: false,
                active: false,
                created_at: Ulid::new().to_string(),
            },
        );
        state
            .meta
            .insert("bootstrap_used".to_owned(), "true".to_owned());
        Ok((device_id, public_key))
    }

    async fn device_key(&self, device_id: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .devices
            .get(device_id)
            .filter(|device| !device.revoked)
            .map(|device| device.public_key.clone()))
    }

    async fn device_user(&self, device_id: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .devices
            .get(device_id)
            .filter(|device| !device.revoked)
            .map(|device| device.user_id.clone()))
    }

    async fn set_active(&self, device_id: &str) -> Result<()> {
        let mut state = self.lock()?;
        let user = state
            .devices
            .get(device_id)
            .filter(|device| !device.revoked)
            .map(|device| device.user_id.clone())
            .context("unknown or revoked device")?;
        for device in state.devices.values_mut() {
            if device.user_id == user {
                device.active = false;
            }
        }
        state
            .devices
            .get_mut(device_id)
            .expect("device was checked above")
            .active = true;
        Ok(())
    }

    async fn revoke_device(&self, device_id: &str) -> Result<bool> {
        let mut state = self.lock()?;
        let Some(device) = state.devices.get_mut(device_id) else {
            return Ok(false);
        };
        if device.revoked {
            return Ok(false);
        }
        device.revoked = true;
        device.active = false;
        Ok(true)
    }

    async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let mut devices = self
            .lock()?
            .devices
            .iter()
            .filter(|(_, device)| device.user_id == "owner")
            .map(|(id, device)| {
                (
                    device.created_at.clone(),
                    DeviceInfo {
                        id: id.clone(),
                        active: device.active,
                        revoked: device.revoked,
                    },
                )
            })
            .collect::<Vec<_>>();
        devices.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(devices.into_iter().map(|(_, device)| device).collect())
    }
}

#[derive(Clone)]
struct MongoRelayStore {
    database: Database,
}

impl MongoRelayStore {
    async fn connect(url: &str, database_name: &str) -> Result<Self> {
        let client = Client::with_uri_str(url)
            .await
            .with_context(|| "connecting to MongoDB for relay metadata")?;
        Ok(MongoRelayStore {
            database: client.database(database_name),
        })
    }

    fn meta_collection(&self) -> Collection<Document> {
        self.database.collection("relay_meta")
    }

    fn devices_collection(&self) -> Collection<Document> {
        self.database.collection("relay_devices")
    }

    fn connectors_collection(&self) -> Collection<Document> {
        self.database.collection("relay_connector_tokens")
    }
}

fn unique_index(keys: Document) -> IndexModel {
    let mut options = IndexOptions::default();
    options.unique = Some(true);
    IndexModel::builder()
        .keys(keys)
        .options(Some(options))
        .build()
}

fn mongo_string(document: &Document, field: &str) -> Result<String> {
    document
        .get_str(field)
        .map(str::to_owned)
        .map_err(|_| anyhow!("MongoDB relay record has no string `{field}` field"))
}

fn mongo_bool(document: &Document, field: &str) -> Result<bool> {
    document
        .get_bool(field)
        .map_err(|_| anyhow!("MongoDB relay record has no boolean `{field}` field"))
}

#[async_trait]
impl RelayStore for MongoRelayStore {
    async fn create_schema(&self) -> Result<()> {
        self.devices_collection()
            .create_indexes(vec![
                unique_index(doc! {"public_key": 1}),
                IndexModel::builder()
                    .keys(doc! {"user_id": 1, "active": 1})
                    .build(),
            ])
            .await?;
        self.connectors_collection()
            .create_index(IndexModel::builder().keys(doc! {"user_id": 1}).build())
            .await?;
        Ok(())
    }

    async fn require_initialised(&self) -> Result<()> {
        self.create_schema().await?;
        if self.meta("bootstrap_hash").await?.is_none() {
            bail!("relay is not initialised; run `ctx relay init` with this MongoDB URL first")
        }
        Ok(())
    }

    async fn meta(&self, key: &str) -> Result<Option<String>> {
        self.meta_collection()
            .find_one(doc! {"_id": key})
            .await?
            .map(|document| mongo_string(&document, "value"))
            .transpose()
    }

    async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.meta_collection()
            .update_one(doc! {"_id": key}, doc! {"$set": {"value": value}})
            .upsert(true)
            .await?;
        Ok(())
    }

    async fn insert_connector(&self, secret: &str, user: &str, label: &str) -> Result<()> {
        self.connectors_collection()
            .insert_one(doc! {
                "_id": hash_secret(secret),
                "user_id": user,
                "label": label,
                "revoked": false,
                "created_at": Ulid::new().to_string(),
            })
            .await?;
        Ok(())
    }

    async fn connector_user(&self, secret: &str) -> Result<Option<String>> {
        self.connectors_collection()
            .find_one(doc! {"_id": hash_secret(secret), "revoked": false})
            .await?
            .map(|document| mongo_string(&document, "user_id"))
            .transpose()
    }

    async fn enrol(&self, code: &str, public_key: &str) -> Result<(String, String)> {
        self.require_initialised().await?;
        let public_key = decode_public_key(public_key).map(|key| encode_public_key(&key))?;
        let expected = self
            .meta("bootstrap_hash")
            .await?
            .context("relay bootstrap state is missing")?;
        if !constant_time_eq(expected.as_bytes(), hash_secret(code).as_bytes()) {
            bail!("bootstrap code is invalid or has already been used")
        }
        let consumed = self
            .meta_collection()
            .update_one(
                doc! {"_id": "bootstrap_used", "value": "false"},
                doc! {"$set": {"value": "true"}},
            )
            .await?;
        if consumed.modified_count != 1 {
            bail!("bootstrap code is invalid or has already been used")
        }
        let device_id = Ulid::new().to_string();
        self.devices_collection()
            .insert_one(doc! {
                "_id": &device_id,
                "user_id": "owner",
                "public_key": &public_key,
                "revoked": false,
                "active": false,
                "created_at": Ulid::new().to_string(),
            })
            .await?;
        Ok((device_id, public_key))
    }

    async fn device_key(&self, device_id: &str) -> Result<Option<String>> {
        self.devices_collection()
            .find_one(doc! {"_id": device_id, "revoked": false})
            .await?
            .map(|document| mongo_string(&document, "public_key"))
            .transpose()
    }

    async fn device_user(&self, device_id: &str) -> Result<Option<String>> {
        self.devices_collection()
            .find_one(doc! {"_id": device_id, "revoked": false})
            .await?
            .map(|document| mongo_string(&document, "user_id"))
            .transpose()
    }

    async fn set_active(&self, device_id: &str) -> Result<()> {
        let user = self
            .device_user(device_id)
            .await?
            .context("unknown or revoked device")?;
        self.devices_collection()
            .update_many(doc! {"user_id": &user}, doc! {"$set": {"active": false}})
            .await?;
        self.devices_collection()
            .update_one(
                doc! {"_id": device_id, "revoked": false},
                doc! {"$set": {"active": true}},
            )
            .await?;
        Ok(())
    }

    async fn revoke_device(&self, device_id: &str) -> Result<bool> {
        let result = self
            .devices_collection()
            .update_one(
                doc! {"_id": device_id, "revoked": false},
                doc! {"$set": {"revoked": true, "active": false}},
            )
            .await?;
        Ok(result.modified_count != 0)
    }

    async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let mut cursor = self
            .devices_collection()
            .find(doc! {"user_id": "owner"})
            .sort(doc! {"created_at": 1})
            .await?;
        let mut devices = Vec::new();
        while let Some(document) = cursor.next().await {
            let document = document?;
            devices.push(DeviceInfo {
                id: mongo_string(&document, "_id")?,
                active: mongo_bool(&document, "active")?,
                revoked: mongo_bool(&document, "revoked")?,
            });
        }
        Ok(devices)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |out, (x, y)| out | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    async fn http_json(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn initialised_memory_store() -> (Arc<MemoryRelayStore>, RelayInit) {
        let store = Arc::new(MemoryRelayStore::default());
        let init = initialise_store(store.as_ref(), Path::new("memory"))
            .await
            .unwrap();
        (store, init)
    }

    fn mcp_request(uri: &str, secret: Option<&str>, id: i64) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(secret) = secret {
            request = request.header("authorization", format!("Bearer {secret}"));
        }
        request
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": id, "method": "ping"}).to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn metadata_store_enrolment_is_one_time_and_connector_secrets_are_scoped() {
        let (db, init) = initialised_memory_store().await;
        assert_eq!(
            db.connector_user(&init.connector_secret)
                .await
                .unwrap()
                .as_deref(),
            Some("owner")
        );
        let key = SigningKey::from_bytes(&[7; 32]);
        let public_key = encode_public_key(&key.verifying_key());
        let (device, saved) = db.enrol(&init.bootstrap_code, &public_key).await.unwrap();
        assert!(!device.is_empty());
        assert_eq!(saved, public_key);
        assert!(db.enrol(&init.bootstrap_code, &public_key).await.is_err());
        assert_eq!(
            db.device_user(&device).await.unwrap().as_deref(),
            Some("owner")
        );
        assert!(db.revoke_device(&device).await.unwrap());
        assert!(db.device_user(&device).await.unwrap().is_none());
    }

    #[test]
    fn sqlite_is_the_durable_default_when_mongodb_is_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let init = init(dir.path()).unwrap();
        assert!(dir.path().join("relay.db").is_file());
        let db = SqliteRelayStore::new(dir.path());
        assert_eq!(
            block_on(db.connector_user(&init.connector_secret))
                .unwrap()
                .as_deref(),
            Some("owner")
        );
    }

    #[test]
    fn only_the_enrolled_private_key_can_answer_a_challenge() {
        let key = SigningKey::from_bytes(&[8; 32]);
        let nonce = [3; 32];
        let device = "01J00000000000000000000000";
        let signature = URL_SAFE_NO_PAD.encode(key.sign(&auth_bytes(device, &nonce)).to_bytes());
        verify_signature(
            &encode_public_key(&key.verifying_key()),
            device,
            &nonce,
            &signature,
        )
        .unwrap();
        assert!(
            verify_signature(
                &encode_public_key(&key.verifying_key()),
                "other",
                &nonce,
                &signature
            )
            .is_err()
        );
    }

    #[test]
    fn relay_and_websocket_urls_follow_the_public_scheme() {
        assert_eq!(
            node_ws_url("https://relay.example").unwrap(),
            "wss://relay.example/v1/node/connect"
        );
        assert_eq!(
            node_ws_url("http://127.0.0.1:8788").unwrap(),
            "ws://127.0.0.1:8788/v1/node/connect"
        );
    }

    #[test]
    fn connector_bearer_header_is_parsed_without_accepting_other_schemes() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer rcm_secret".parse().unwrap());
        assert_eq!(bearer_secret(&headers), Some("rcm_secret"));
        headers.insert("authorization", "Basic rcm_secret".parse().unwrap());
        assert_eq!(bearer_secret(&headers), None);
    }

    #[test]
    fn node_configuration_remembers_the_branch_it_serves() {
        let dir = tempfile::tempdir().unwrap();
        let home = CtxHome::at(dir.path().join("ctx"));
        let config = NodeConfig {
            relay_url: "https://relay.example".into(),
            device_id: "device".into(),
            branch: "project/code".into(),
        };
        save_node_config(&home, &config).unwrap();
        let loaded = load_node_config(&home).unwrap().unwrap();
        assert_eq!(loaded.branch, "project/code");
    }

    #[tokio::test]
    async fn mcp_endpoint_rejects_missing_and_invalid_connector_credentials() {
        let (store, init) = initialised_memory_store().await;
        let relay = router(RelayState::new(store));

        let (status, body) = http_json(&relay, mcp_request("/mcp", None, 1)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], -32001);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Missing")
        );

        let (status, body) = http_json(&relay, mcp_request("/mcp", Some("rcm_wrong"), 2)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], -32001);

        // The generated credential itself is not a valid request without a
        // connected node, but it passes authentication and reports offline.
        let (status, body) =
            http_json(&relay, mcp_request("/mcp", Some(&init.connector_secret), 3)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn authenticated_node_receives_and_answers_relay_requests() {
        let (store, init) = initialised_memory_store().await;
        let state = RelayState::new(store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router(server_state)).await;
        });

        let signing_key = SigningKey::from_bytes(&[9; 32]);
        let (mut node, _) = connect_async(format!("ws://{address}/v1/node/connect"))
            .await
            .unwrap();
        send_client_wire(
            &mut node,
            &Wire::Enroll {
                bootstrap_code: init.bootstrap_code,
                public_key: encode_public_key(&signing_key.verifying_key()),
            },
        )
        .await
        .unwrap();
        authenticate_node(&mut node, &signing_key).await.unwrap();

        for _ in 0..20 {
            if state.nodes.read().await.contains_key("owner") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(state.nodes.read().await.contains_key("owner"));

        let responder = tokio::spawn(async move {
            let frame = node.next().await.unwrap().unwrap();
            let ClientMessage::Text(text) = frame else {
                panic!("relay sent a non-text request")
            };
            let Wire::Request {
                request_id,
                payload,
            } = serde_json::from_str::<Wire>(&text).unwrap()
            else {
                panic!("relay sent the wrong envelope")
            };
            send_client_wire(
                &mut node,
                &Wire::Response {
                    request_id,
                    payload: json!({"echo": payload}),
                },
            )
            .await
            .unwrap();
        });

        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let response = state.forward("owner", request.clone()).await.unwrap();
        assert_eq!(response, json!({"echo": request}));
        responder.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn http_mcp_forwards_over_an_authenticated_node_and_honours_revocation() {
        let (store, init) = initialised_memory_store().await;
        let state = RelayState::new(store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router(server_state)).await;
        });

        let signing_key = SigningKey::from_bytes(&[10; 32]);
        let (mut node, _) = connect_async(format!("ws://{address}/v1/node/connect"))
            .await
            .unwrap();
        send_client_wire(
            &mut node,
            &Wire::Enroll {
                bootstrap_code: init.bootstrap_code.clone(),
                public_key: encode_public_key(&signing_key.verifying_key()),
            },
        )
        .await
        .unwrap();
        let device_id = authenticate_node(&mut node, &signing_key).await.unwrap();
        for _ in 0..20 {
            if state.nodes.read().await.contains_key("owner") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let responder = tokio::spawn(async move {
            for _ in 0..2 {
                let ClientMessage::Text(text) = node.next().await.unwrap().unwrap() else {
                    panic!("relay sent a non-text request")
                };
                let Wire::Request {
                    request_id,
                    payload,
                } = serde_json::from_str::<Wire>(&text).unwrap()
                else {
                    panic!("relay sent the wrong envelope")
                };
                send_client_wire(
                    &mut node,
                    &Wire::Response {
                        request_id,
                        payload: json!({"ok": payload["id"]}),
                    },
                )
                .await
                .unwrap();
            }
        });

        let relay = router(state.clone());
        let (status, body) =
            http_json(&relay, mcp_request("/mcp", Some(&init.connector_secret), 7)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": 7}));

        let (status, body) = http_json(
            &relay,
            mcp_request(&format!("/mcp/{}", init.connector_secret), None, 8),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": 8}));
        responder.await.unwrap();

        assert!(state.db.revoke_device(&device_id).await.unwrap());
        let (status, body) =
            http_json(&relay, mcp_request("/mcp", Some(&init.connector_secret), 9)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], -32002);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("offline")
        );
        server.abort();
    }
}
