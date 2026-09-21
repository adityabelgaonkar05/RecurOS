//! A small, self-hostable MCP relay and its local HTTP executor.
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::StreamExt;
use mongodb::bson::{Document, doc};
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, Database, IndexModel};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use ulid::Ulid;
use url::{Host, Url};

use ctx_app::App;
use ctx_core::BranchRef;
use ctx_git::CtxHome;

pub const DEFAULT_RELAY_ADDR: &str = "127.0.0.1:8788";
const NODE_DIR: &str = ".node";
const NODE_CONFIG: &str = "relay.json";
const NODE_KEY: &str = "device.ed25519";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const NODE_REGISTRATION_TTL: Duration = Duration::from_secs(90);
const NODE_HEARTBEAT: Duration = Duration::from_secs(30);
const FORWARD_CLOCK_SKEW: i64 = 60;
const RELAY_SIGNING_KEY_META: &str = "relay_signing_key";

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

/// Serve the public relay. TLS belongs at the public reverse proxy.
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
        let state = RelayState::new(db.clone(), relay_signing_key(db.as_ref()).await?);
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
    let key = load_or_create_key(home)?;
    let relay_url = normalise_relay_url(relay_url)?;
    let response = block_on(async {
        let response = reqwest::Client::new()
            .post(format!("{relay_url}/v1/node/enroll"))
            .json(&EnrollRequest {
                bootstrap_code: bootstrap_code.to_owned(),
                public_key: encode_public_key(&key.verifying_key()),
            })
            .send()
            .await?;
        if !response.status().is_success() {
            bail!(
                "relay rejected enrolment: {}",
                response.text().await.unwrap_or_default()
            )
        }
        Ok(response.json::<EnrollResponse>().await?)
    })?;
    decode_public_key(&response.relay_public_key)
        .context("relay returned an invalid signing key")?;
    save_node_config(
        home,
        &NodeConfig {
            relay_url,
            device_id: response.device_id.clone(),
            relay_public_key: response.relay_public_key,
            branch: branch.to_string(),
        },
    )?;
    Ok(response.device_id)
}

/// Keep the local context node reachable by remote MCP clients. It owns the
/// execution path; the relay only carries request/response envelopes.
pub fn node_start(
    home: CtxHome,
    relay_override: Option<&str>,
    branch_override: Option<&str>,
    public_url: Option<&str>,
    listen: &str,
) -> Result<()> {
    let mut config = load_node_config(&home)?.context(
        "node is not paired yet; run `ctx node login --relay URL --code BOOTSTRAP_CODE`",
    )?;
    if let Some(url) = relay_override {
        config.relay_url = normalise_relay_url(url)?;
    }
    if let Some(branch) = branch_override {
        config.branch = BranchRef::new(branch)?.to_string();
    }
    if config.relay_public_key.is_empty() {
        bail!("this node was paired with the legacy relay protocol; run `ctx node login` again")
    }
    if relay_override.is_some() || branch_override.is_some() {
        save_node_config(&home, &config)?;
    }
    let key = load_or_create_key(&home)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_node(home, config, key, public_url, listen))
}

/// Report only non-sensitive node configuration for `ctx status` and users.
pub fn node_status(home: &CtxHome) -> Result<Option<(String, String, String)>> {
    Ok(load_node_config(home)?.map(|c| (c.relay_url, c.device_id, c.branch)))
}

#[derive(Clone)]
struct RelayState {
    db: Arc<dyn RelayStore>,
    signing_key: Arc<SigningKey>,
    client: reqwest::Client,
}

impl RelayState {
    fn new(db: Arc<dyn RelayStore>, signing_key: SigningKey) -> Self {
        RelayState {
            db,
            signing_key: Arc::new(signing_key),
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("valid relay HTTP client"),
        }
    }

    async fn forward(&self, user: &str, body: Vec<u8>) -> Result<Value, RelayError> {
        let target = self
            .db
            .active_node(user)
            .await
            .map_err(RelayError::Storage)?;
        let Some(target) = target.filter(|target| target.expires_at >= unix_seconds()) else {
            return Err(RelayError::NodeOffline);
        };
        let endpoint = node_execute_url(&target.url).map_err(RelayError::Other)?;
        let timestamp = unix_seconds();
        let nonce = URL_SAFE_NO_PAD.encode(random_bytes(16).map_err(RelayError::Other)?);
        let signature =
            self.signing_key
                .sign(&forward_bytes(&target.device_id, timestamp, &nonce, &body));
        let response = self
            .client
            .post(endpoint)
            .header("content-type", "application/json")
            .header("x-ctx-device", &target.device_id)
            .header("x-ctx-timestamp", timestamp.to_string())
            .header("x-ctx-nonce", &nonce)
            .header(
                "x-ctx-signature",
                URL_SAFE_NO_PAD.encode(signature.to_bytes()),
            )
            .body(body)
            .send()
            .await
            .map_err(|_| RelayError::NodeOffline)?;
        if !response.status().is_success() {
            return Err(RelayError::NodeOffline);
        }
        response
            .json::<Value>()
            .await
            .map_err(|_| RelayError::NodeOffline)
    }
}

#[derive(Debug)]
enum RelayError {
    NodeOffline,
    Storage(anyhow::Error),
    Other(anyhow::Error),
}

impl RelayError {
    fn message(&self) -> &'static str {
        match self {
            RelayError::NodeOffline => {
                "The RecurOS node for this account is offline. Start `ctx node start` on the paired device."
            }
            RelayError::Storage(_) | RelayError::Other(_) => {
                "The RecurOS relay could not reach the active node."
            }
        }
    }
}

fn router(state: RelayState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/node/enroll", post(node_enroll))
        .route("/v1/node/register", post(node_register))
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

#[derive(Serialize, Deserialize)]
struct EnrollRequest {
    bootstrap_code: String,
    public_key: String,
}

#[derive(Serialize, Deserialize)]
struct EnrollResponse {
    device_id: String,
    relay_public_key: String,
}

#[derive(Deserialize)]
struct RegisterRequest {
    device_id: String,
    public_url: String,
    expires_at: i64,
    signature: String,
}

async fn node_enroll(
    State(state): State<RelayState>,
    Json(request): Json<EnrollRequest>,
) -> Response {
    let outcome = async {
        let (device_id, _) = state
            .db
            .enrol(&request.bootstrap_code, &request.public_key)
            .await?;
        Ok::<_, anyhow::Error>(Json(EnrollResponse {
            device_id,
            relay_public_key: encode_public_key(&state.signing_key.verifying_key()),
        }))
    }
    .await;
    match outcome {
        Ok(response) => response.into_response(),
        Err(error) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn node_register(
    State(state): State<RelayState>,
    Json(request): Json<RegisterRequest>,
) -> Response {
    let outcome = async {
        let public_url = normalise_public_node_url(&request.public_url)?;
        let now = unix_seconds();
        if request.expires_at <= now
            || request.expires_at > now + NODE_REGISTRATION_TTL.as_secs() as i64 + 30
        {
            bail!("registration expiry must be within the next 120 seconds")
        }
        let public_key = state
            .db
            .device_key(&request.device_id)
            .await?
            .context("unknown or revoked device")?;
        verify_registration(
            &public_key,
            &request.device_id,
            &public_url,
            request.expires_at,
            &request.signature,
        )?;
        state
            .db
            .set_node_target(&request.device_id, &public_url, request.expires_at)
            .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match outcome {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(error) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn mcp(headers: HeaderMap, State(state): State<RelayState>, body: Bytes) -> Response {
    forward_mcp(state, bearer_secret(&headers), body).await
}

async fn mcp_with_path_secret(
    AxumPath(secret): AxumPath<String>,
    State(state): State<RelayState>,
    body: Bytes,
) -> Response {
    forward_mcp(state, Some(secret.as_str()), body).await
}

async fn forward_mcp(state: RelayState, secret: Option<&str>, body: Bytes) -> Response {
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(rpc_error(&Value::Null, -32700, "Invalid JSON-RPC body.")),
            )
                .into_response();
        }
    };
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
    match state.forward(&user, body.to_vec()).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => {
            if let RelayError::Storage(cause) | RelayError::Other(cause) = &error {
                eprintln!("ctx relay: forwarding error: {cause:#}");
            }
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(rpc_error(&payload, -32002, error.message())),
            )
                .into_response()
        }
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

async fn run_node(
    home: CtxHome,
    config: NodeConfig,
    key: SigningKey,
    supplied_url: Option<&str>,
    listen: &str,
) -> Result<()> {
    let mut app = App::open(home, None)?;
    app.set_active(&BranchRef::new(&config.branch)?)?;
    let relay_key = decode_public_key(&config.relay_public_key)?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding local node at {listen}"))?;
    let local_addr = listener.local_addr()?;
    if !local_addr.ip().is_loopback() {
        bail!("the local node executor must bind a loopback address, not {local_addr}")
    }
    let executor = ExecutorState {
        app: Arc::new(AsyncMutex::new(app)),
        device_id: config.device_id.clone(),
        relay_key,
        used_nonces: Arc::new(AsyncMutex::new(HashMap::new())),
    };
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/execute", post(execute_local))
                .with_state(executor),
        )
        .await
    });
    let (public_url, mut tunnel) = match supplied_url {
        Some(url) => (normalise_public_node_url(url)?, None),
        None => {
            let (child, url) = start_quick_tunnel(local_addr).await?;
            eprintln!("ctx node: Cloudflare Quick Tunnel is {url}");
            (url, Some(child))
        }
    };
    let result = async {
        register_node(&config, &key, &public_url).await?;
        eprintln!(
            "ctx node: registered {public_url} with {}",
            config.relay_url
        );
        let mut heartbeat = tokio::time::interval(NODE_HEARTBEAT);
        heartbeat.tick().await;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("ctx node: stopping");
                    return Ok(());
                }
                _ = heartbeat.tick() => {
                    if let Some(child) = tunnel.as_mut()
                        && child.try_wait()?.is_some() {
                        bail!("cloudflared exited; start `ctx node start` again")
                    }
                    register_node(&config, &key, &public_url).await?;
                }
            }
        }
    }
    .await;
    if let Some(mut child) = tunnel {
        let _ = child.kill().await;
    }
    server.abort();
    result
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

#[derive(Clone)]
struct ExecutorState {
    app: Arc<AsyncMutex<App>>,
    device_id: String,
    relay_key: VerifyingKey,
    used_nonces: Arc<AsyncMutex<HashMap<String, i64>>>,
}

async fn execute_local(
    headers: HeaderMap,
    State(state): State<ExecutorState>,
    body: Bytes,
) -> Response {
    if let Err(error) = verify_forward_request(&state, &headers, &body).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": error.to_string()})),
        )
            .into_response();
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(rpc_error(&Value::Null, -32700, "Invalid JSON-RPC body.")),
            )
                .into_response();
        }
    };
    Json(execute_mcp(&mut *state.app.lock().await, payload)).into_response()
}

async fn verify_forward_request(
    state: &ExecutorState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<()> {
    let device = header(headers, "x-ctx-device")?;
    if device != state.device_id {
        bail!("request targets another device")
    }
    let timestamp: i64 = header(headers, "x-ctx-timestamp")?
        .parse()
        .context("invalid relay timestamp")?;
    if (unix_seconds() - timestamp).abs() > FORWARD_CLOCK_SKEW {
        bail!("relay request has expired")
    }
    let nonce = header(headers, "x-ctx-nonce")?;
    if nonce.len() < 16 || URL_SAFE_NO_PAD.decode(nonce.as_bytes()).is_err() {
        bail!("invalid relay nonce")
    }
    let signature = Signature::from_slice(
        &URL_SAFE_NO_PAD.decode(header(headers, "x-ctx-signature")?.as_bytes())?,
    )?;
    state
        .relay_key
        .verify(&forward_bytes(device, timestamp, nonce, body), &signature)
        .map_err(|_| anyhow!("invalid relay signature"))?;
    let mut nonces = state.used_nonces.lock().await;
    nonces.retain(|_, seen| *seen >= unix_seconds() - FORWARD_CLOCK_SKEW);
    if nonces.insert(nonce.to_owned(), timestamp).is_some() {
        bail!("replayed relay request")
    }
    Ok(())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str> {
    headers
        .get(name)
        .context(format!("missing {name}"))?
        .to_str()
        .context(format!("invalid {name}"))
}

#[derive(Serialize)]
struct RegisterRequestOut<'a> {
    device_id: &'a str,
    public_url: &'a str,
    expires_at: i64,
    signature: String,
}

async fn register_node(config: &NodeConfig, key: &SigningKey, public_url: &str) -> Result<()> {
    let expires_at = unix_seconds() + NODE_REGISTRATION_TTL.as_secs() as i64;
    let signature = key.sign(&registration_bytes(
        &config.device_id,
        public_url,
        expires_at,
    ));
    let response = reqwest::Client::new()
        .post(format!("{}/v1/node/register", config.relay_url))
        .json(&RegisterRequestOut {
            device_id: &config.device_id,
            public_url,
            expires_at,
            signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        })
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "relay rejected node registration: {}",
            response.text().await.unwrap_or_default()
        )
    }
    Ok(())
}

async fn start_quick_tunnel(local_addr: std::net::SocketAddr) -> Result<(Child, String)> {
    let mut child = Command::new("cloudflared")
        .args(["tunnel", "--url", &format!("http://{local_addr}")])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(
            "starting cloudflared; install it or pass --public-url for ngrok/a named tunnel",
        )?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    if let Some(stream) = child.stdout.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }
    if let Some(stream) = child.stderr.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }
    drop(tx);
    let url = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = rx.recv().await {
            if let Some(url) = quick_tunnel_url(&line) {
                return Ok(url);
            }
        }
        bail!("cloudflared exited before providing a tunnel URL")
    })
    .await
    .context("waiting for a Cloudflare Quick Tunnel URL")??;
    Ok((child, url))
}

fn quick_tunnel_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let raw = &line[start..];
    let end = raw
        .find(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\\')
        .unwrap_or(raw.len());
    let candidate = &raw[..end];
    if Url::parse(candidate)
        .ok()?
        .host_str()?
        .ends_with(".trycloudflare.com")
    {
        normalise_public_node_url(candidate).ok()
    } else {
        None
    }
}

fn verify_registration(
    public_key: &str,
    device_id: &str,
    public_url: &str,
    expires_at: i64,
    signature: &str,
) -> Result<()> {
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature.as_bytes())?)?;
    decode_public_key(public_key)?
        .verify(
            &registration_bytes(device_id, public_url, expires_at),
            &signature,
        )
        .map_err(|_| anyhow!("invalid device registration signature"))
}

fn registration_bytes(device_id: &str, public_url: &str, expires_at: i64) -> Vec<u8> {
    let mut out = b"recuros-node-register-v1\0".to_vec();
    out.extend_from_slice(device_id.as_bytes());
    out.push(0);
    out.extend_from_slice(public_url.as_bytes());
    out.push(0);
    out.extend_from_slice(&expires_at.to_be_bytes());
    out
}

fn forward_bytes(device_id: &str, timestamp: i64, nonce: &str, body: &[u8]) -> Vec<u8> {
    let mut out = b"recuros-node-forward-v1\0".to_vec();
    out.extend_from_slice(device_id.as_bytes());
    out.push(0);
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(nonce.as_bytes());
    out.push(0);
    out.extend_from_slice(blake3::hash(body).as_bytes());
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
    #[serde(default)]
    relay_public_key: String,
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
    let url = Url::parse(raw.trim()).context("relay URL must be a valid URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("relay URL must start with http:// or https://")
    }
    Ok(raw.trim().trim_end_matches('/').to_owned())
}

fn normalise_public_node_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw.trim()).context("public node URL must be a valid HTTPS URL")?;
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("public node URL must be a plain HTTPS URL")
    }
    match url.host() {
        Some(Host::Domain(host)) if !host.eq_ignore_ascii_case("localhost") => {}
        _ => {
            bail!("public node URL must use a public DNS hostname, not an IP address or localhost")
        }
    }
    if url.path() != "/" && !url.path().is_empty() {
        bail!("public node URL must not include a path")
    }
    url.set_path("/");
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn node_execute_url(node_url: &str) -> Result<Url> {
    let base = Url::parse(node_url).context("registered node URL is invalid")?;
    base.join("v1/execute")
        .context("registered node URL cannot be used")
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
    async fn set_node_target(&self, device_id: &str, url: &str, expires_at: i64) -> Result<()>;
    async fn active_node(&self, user: &str) -> Result<Option<NodeTarget>>;
    async fn revoke_device(&self, device_id: &str) -> Result<bool>;
    async fn devices(&self) -> Result<Vec<DeviceInfo>>;
}

#[derive(Debug, Clone)]
struct NodeTarget {
    device_id: String,
    url: String,
    expires_at: i64,
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
    let signing_key =
        SigningKey::from_bytes(&random_bytes(32)?.try_into().expect("32 bytes requested"));
    store
        .set_meta(
            RELAY_SIGNING_KEY_META,
            &URL_SAFE_NO_PAD.encode(signing_key.to_bytes()),
        )
        .await?;
    store
        .insert_connector(&connector_secret, "owner", "initial connector")
        .await?;
    Ok(RelayInit {
        bootstrap_code,
        connector_secret,
    })
}

async fn relay_signing_key(store: &dyn RelayStore) -> Result<SigningKey> {
    if let Some(saved) = store.meta(RELAY_SIGNING_KEY_META).await? {
        let bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(saved.as_bytes())?
            .try_into()
            .map_err(|_| anyhow!("relay signing key has the wrong length"))?;
        return Ok(SigningKey::from_bytes(&bytes));
    }
    // A relay created by a pre-HTTP release gains a signing key lazily. The
    // first node must pair again to learn its public half.
    let bytes: [u8; 32] = random_bytes(32)?.try_into().expect("32 bytes requested");
    store
        .set_meta(RELAY_SIGNING_KEY_META, &URL_SAFE_NO_PAD.encode(bytes))
        .await?;
    Ok(SigningKey::from_bytes(&bytes))
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
              node_url TEXT, node_expires_at INTEGER,
              created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS connector_tokens (
              token_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL, label TEXT NOT NULL,
              revoked INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS devices_active ON devices(user_id, active);
            ",
        )?;
        let connection = self.connection()?;
        let columns = connection
            .prepare("PRAGMA table_info(devices)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if !columns.iter().any(|column| column == "node_url") {
            connection.execute("ALTER TABLE devices ADD COLUMN node_url TEXT", [])?;
        }
        if !columns.iter().any(|column| column == "node_expires_at") {
            connection.execute("ALTER TABLE devices ADD COLUMN node_expires_at INTEGER", [])?;
        }
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

    async fn set_node_target(&self, device_id: &str, url: &str, expires_at: i64) -> Result<()> {
        let user: String = self.connection()?.query_row(
            "SELECT user_id FROM devices WHERE id = ? AND revoked = 0",
            [device_id],
            |row| row.get(0),
        )?;
        let conn = self.connection()?;
        conn.execute("UPDATE devices SET active = 0, node_url = NULL, node_expires_at = NULL WHERE user_id = ?", [user])?;
        conn.execute(
            "UPDATE devices SET active = 1, node_url = ?, node_expires_at = ? WHERE id = ?",
            params![url, expires_at, device_id],
        )?;
        Ok(())
    }

    async fn active_node(&self, user: &str) -> Result<Option<NodeTarget>> {
        Ok(self.connection()?.query_row(
            "SELECT id, node_url, node_expires_at FROM devices WHERE user_id = ? AND revoked = 0 AND active = 1 AND node_url IS NOT NULL AND node_expires_at IS NOT NULL",
            [user],
            |row| Ok(NodeTarget { device_id: row.get(0)?, url: row.get(1)?, expires_at: row.get(2)? }),
        ).optional()?)
    }

    async fn revoke_device(&self, device_id: &str) -> Result<bool> {
        let changed = self.connection()?.execute(
            "UPDATE devices SET revoked = 1, active = 0, node_url = NULL, node_expires_at = NULL WHERE id = ? AND revoked = 0",
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
    node_url: Option<String>,
    node_expires_at: Option<i64>,
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
                node_url: None,
                node_expires_at: None,
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

    async fn set_node_target(&self, device_id: &str, url: &str, expires_at: i64) -> Result<()> {
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
                device.node_url = None;
                device.node_expires_at = None;
            }
        }
        state
            .devices
            .get_mut(device_id)
            .expect("device was checked above")
            .active = true;
        let device = state
            .devices
            .get_mut(device_id)
            .expect("device was checked above");
        device.node_url = Some(url.to_owned());
        device.node_expires_at = Some(expires_at);
        Ok(())
    }

    async fn active_node(&self, user: &str) -> Result<Option<NodeTarget>> {
        Ok(self.lock()?.devices.iter().find_map(|(id, device)| {
            if device.revoked || !device.active || device.user_id != user {
                return None;
            }
            Some(NodeTarget {
                device_id: id.clone(),
                url: device.node_url.clone()?,
                expires_at: device.node_expires_at?,
            })
        }))
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
        device.node_url = None;
        device.node_expires_at = None;
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

    async fn set_node_target(&self, device_id: &str, url: &str, expires_at: i64) -> Result<()> {
        let user = self
            .device_user(device_id)
            .await?
            .context("unknown or revoked device")?;
        self.devices_collection()
            .update_many(
                doc! {"user_id": &user},
                doc! {"$set": {"active": false}, "$unset": {"node_url": "", "node_expires_at": ""}},
            )
            .await?;
        self.devices_collection()
            .update_one(
                doc! {"_id": device_id, "revoked": false},
                doc! {"$set": {"active": true, "node_url": url, "node_expires_at": expires_at}},
            )
            .await?;
        Ok(())
    }

    async fn active_node(&self, user: &str) -> Result<Option<NodeTarget>> {
        self.devices_collection()
            .find_one(doc! {"user_id": user, "revoked": false, "active": true, "node_url": {"$exists": true}, "node_expires_at": {"$exists": true}})
            .await?
            .map(|document| Ok(NodeTarget {
                device_id: mongo_string(&document, "_id")?,
                url: mongo_string(&document, "node_url")?,
                expires_at: document.get_i64("node_expires_at").map_err(|_| anyhow!("MongoDB relay record has no integer `node_expires_at` field"))?,
            }))
            .transpose()
    }

    async fn revoke_device(&self, device_id: &str) -> Result<bool> {
        let result = self
            .devices_collection()
            .update_one(
                doc! {"_id": device_id, "revoked": false},
                doc! {"$set": {"revoked": true, "active": false}, "$unset": {"node_url": "", "node_expires_at": ""}},
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
    fn only_the_enrolled_private_key_can_register_a_url() {
        let key = SigningKey::from_bytes(&[8; 32]);
        let device = "01J00000000000000000000000";
        let url = "https://random.trycloudflare.com";
        let expiry = 1234;
        let signature = URL_SAFE_NO_PAD.encode(
            key.sign(&registration_bytes(device, url, expiry))
                .to_bytes(),
        );
        verify_registration(
            &encode_public_key(&key.verifying_key()),
            device,
            url,
            expiry,
            &signature,
        )
        .unwrap();
        assert!(
            verify_registration(
                &encode_public_key(&key.verifying_key()),
                "other",
                url,
                expiry,
                &signature
            )
            .is_err()
        );
    }

    #[test]
    fn relay_and_node_urls_are_validated() {
        assert_eq!(
            normalise_relay_url("https://relay.example/").unwrap(),
            "https://relay.example"
        );
        assert_eq!(
            normalise_public_node_url("https://node.example/").unwrap(),
            "https://node.example"
        );
        assert!(normalise_public_node_url("http://node.example").is_err());
        assert!(normalise_public_node_url("https://127.0.0.1").is_err());
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
            relay_public_key: "relay-key".into(),
            branch: "project/code".into(),
        };
        save_node_config(&home, &config).unwrap();
        let loaded = load_node_config(&home).unwrap().unwrap();
        assert_eq!(loaded.branch, "project/code");
    }

    #[tokio::test]
    async fn mcp_endpoint_rejects_missing_and_invalid_connector_credentials() {
        let (store, init) = initialised_memory_store().await;
        let relay = router(RelayState::new(store, SigningKey::from_bytes(&[3; 32])));

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

    async fn signed_test_node(
        State(key): State<VerifyingKey>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let device = header(&headers, "x-ctx-device").unwrap();
        let timestamp: i64 = header(&headers, "x-ctx-timestamp")
            .unwrap()
            .parse()
            .unwrap();
        let nonce = header(&headers, "x-ctx-nonce").unwrap();
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(header(&headers, "x-ctx-signature").unwrap().as_bytes())
                .unwrap(),
        )
        .unwrap();
        key.verify(&forward_bytes(device, timestamp, nonce, &body), &signature)
            .unwrap();
        Json(json!({"forwarded": serde_json::from_slice::<Value>(&body).unwrap()["id"]}))
            .into_response()
    }

    #[tokio::test]
    async fn registered_http_node_receives_a_signed_mcp_request() {
        let (store, init) = initialised_memory_store().await;
        let device_key = SigningKey::from_bytes(&[11; 32]);
        let (device, _) = store
            .enrol(
                &init.bootstrap_code,
                &encode_public_key(&device_key.verifying_key()),
            )
            .await
            .unwrap();
        let relay_key = SigningKey::from_bytes(&[12; 32]);
        let node_relay_key = relay_key.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/execute", post(signed_test_node))
                    .with_state(node_relay_key.verifying_key()),
            )
            .await
            .unwrap();
        });
        store
            .set_node_target(&device, &format!("http://{address}"), unix_seconds() + 60)
            .await
            .unwrap();
        let relay = router(RelayState::new(store, relay_key));
        let (status, body) = http_json(
            &relay,
            mcp_request("/mcp", Some(&init.connector_secret), 42),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"forwarded": 42}));
        server.abort();
    }

    #[tokio::test]
    async fn registration_endpoint_requires_a_valid_device_signature_and_activates_target() {
        let (store, init) = initialised_memory_store().await;
        let device_key = SigningKey::from_bytes(&[13; 32]);
        let (device, _) = store
            .enrol(
                &init.bootstrap_code,
                &encode_public_key(&device_key.verifying_key()),
            )
            .await
            .unwrap();
        let relay = router(RelayState::new(
            store.clone(),
            SigningKey::from_bytes(&[14; 32]),
        ));
        let url = "https://node.trycloudflare.com";
        let expiry = unix_seconds() + 60;
        let signature = URL_SAFE_NO_PAD.encode(
            device_key
                .sign(&registration_bytes(&device, url, expiry))
                .to_bytes(),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/node/register")
            .header("content-type", "application/json")
            .body(Body::from(json!({"device_id": device, "public_url": url, "expires_at": expiry, "signature": signature}).to_string()))
            .unwrap();
        let (status, body) = http_json(&relay, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(store.active_node("owner").await.unwrap().unwrap().url, url);

        let invalid = Request::builder()
            .method("POST")
            .uri("/v1/node/register")
            .header("content-type", "application/json")
            .body(Body::from(json!({"device_id": device, "public_url": "https://attacker.example", "expires_at": expiry, "signature": signature}).to_string()))
            .unwrap();
        let (status, _) = http_json(&relay, invalid).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
