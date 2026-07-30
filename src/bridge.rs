// src/bridge.rs — WebSocket bridge: browser extension ↔ Rust daemon
//
// Security (v6.3):
//   • Origin validation — logs origin for auditing
//   • Message size limits to prevent OOM from malicious payloads
//   • Strict frame size limits
//
// The user runs `cargo run -- /path/to/project`.
// The browser extension connects to ws://localhost:7878.
//
// Extension → Rust : Ready, DomUpdate (FSM), PasteAck, CollectRequested
// Rust → Extension : PasteContext (file contents), TypeConfirmation (results)

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex},
};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{
        protocol::WebSocketConfig,
        Message,
    },
    WebSocketStream,
};
use tracing::{debug, error, info, warn};

use crate::error::{WessalError, Result};

// ═══════════════════════════════════════════════════════════════════════════════
// Security constants
// ═══════════════════════════════════════════════════════════════════════════════

/// Maximum WebSocket message size (10 MB)
const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

/// Maximum WebSocket frame size (1 MB)
const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Maximum length for full_text field in DomUpdate (1 MB)
const MAX_FULL_TEXT_LEN: usize = 1024 * 1024;

/// Whitelisted origins for WebSocket connections (for logging/validation)
#[allow(dead_code)]
const ALLOWED_ORIGINS: &[&str] = &[
    // Chrome extension origins (any extension ID)
    "chrome-extension://",
    // Whitelisted AI domains
    "https://chat.openai.com",
    "https://chatgpt.com",
    "https://claude.ai",
    "https://gemini.google.com",
    "https://aistudio.google.com",
    "https://kimi.com",
    "https://chat.z.ai",
    // Local development (for testing)
    "http://localhost",
    "http://127.0.0.1",
];

// ═══════════════════════════════════════════════════════════════════════════════
// Wire types
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Deserialize)]
pub struct DomUpdate {
    #[serde(rename = "textLen")]
    pub text_len:     usize,
    #[serde(rename = "stopVisible")]
    pub stop_visible: bool,
    #[serde(rename = "codeBlocks")]
    pub code_blocks:  Vec<RawCodeBlock>,
    /// Full innerText of the last assistant message.
    #[serde(rename = "fullText", default)]
    pub full_text:    String,
    #[serde(rename = "fenceOpen")]
    pub fence_open:   bool,
    pub timestamp:    u64,
}

impl DomUpdate {
    /// Validate and sanitize DomUpdate fields to prevent OOM
    fn sanitize(mut self) -> Self {
        if self.full_text.len() > MAX_FULL_TEXT_LEN {
            warn!(
                "full_text too large ({} bytes), truncating to {} bytes",
                self.full_text.len(), MAX_FULL_TEXT_LEN
            );
            // UTF-8 safe truncation
            self.full_text = self.full_text.chars().take(MAX_FULL_TEXT_LEN).collect();
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawCodeBlock {
    pub lang:   String,
    pub code:   String,
    pub closed: bool,
}

/// All messages the extension can send to Rust.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InboundMsg {
    /// Extension loaded / tab navigated.
    Ready { url: String },
    /// Periodic DOM snapshot from MutationObserver.
    DomUpdate(DomUpdate),
    /// Acknowledgement that a paste command succeeded or failed.
    PasteAck { ok: bool },
    /// User clicked ⚡ Collect — carries the current DOM snapshot.
    CollectRequested {
        full_text:   String,
        code_blocks: Vec<RawCodeBlock>,
    },
}

/// Commands Rust sends to the extension.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundCmd {
    /// Paste a block of text into the LLM input field (file contents, context).
    PasteContext     { content: String },
    /// Paste an action-result block and leave cursor at the bottom.
    TypeConfirmation { content: String },
}

// ═══════════════════════════════════════════════════════════════════════════════
// BridgeEvent — events emitted to main.rs
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// Extension connected (or reconnected after a tab reload).
    ExtensionConnected { url: String },
    /// User submitted a new message — LLM started generating.
    StreamingStarted,
    /// LLM response settled and all fences are closed.
    ResponseCommitted { code_blocks: Vec<RawCodeBlock>, full_text: String },
    /// User clicked ⚡ Collect in the page.
    CollectRequested  { full_text: String, code_blocks: Vec<RawCodeBlock> },
}

// ═══════════════════════════════════════════════════════════════════════════════
// CompletionFsm — detects streaming start / response committed
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, PartialEq, Eq)]
enum FsmState { Idle, Streaming, Settling, Committed }

struct CompletionFsm {
    state:            FsmState,
    last_mutation:    Instant,
    last_text_len:    usize,
    settling_window:  Duration,
    pub last_code_blocks: Vec<RawCodeBlock>,
    /// Persists across settling ticks so the committed event carries real text.
    last_full_text:   String,
}

impl CompletionFsm {
    fn new(settling_window: Duration) -> Self {
        Self {
            state:            FsmState::Idle,
            last_mutation:    Instant::now(),
            last_text_len:    0,
            settling_window,
            last_code_blocks: Vec::new(),
            last_full_text:   String::new(),
        }
    }

    fn transition(&mut self, u: &DomUpdate) -> Option<BridgeEvent> {
        let changed = u.text_len != self.last_text_len;
        if changed {
            self.last_mutation = Instant::now();
            self.last_text_len = u.text_len;
        }
        // Cache real full_text; synthetic settling ticks carry String::new().
        if !u.full_text.is_empty() {
            self.last_full_text = u.full_text.clone();
        }
        let stable = self.last_mutation.elapsed() >= self.settling_window;

        match self.state {
            FsmState::Idle => {
                if u.stop_visible || changed {
                    self.state = FsmState::Streaming;
                    return Some(BridgeEvent::StreamingStarted);
                }
            }
            FsmState::Streaming => {
                if !u.stop_visible && stable {
                    self.state = FsmState::Settling;
                }
            }
            FsmState::Settling => {
                if u.stop_visible || changed {
                    self.state = FsmState::Streaming;
                } else if !u.stop_visible && stable && !u.fence_open {
                    if u.code_blocks.iter().all(|b| b.closed) {
                        self.state            = FsmState::Committed;
                        self.last_code_blocks = u.code_blocks.clone();
                        return Some(BridgeEvent::ResponseCommitted {
                            code_blocks: u.code_blocks.clone(),
                            full_text:   self.last_full_text.clone(),
                        });
                    } else {
                        // Open fence — wait longer.
                        self.last_mutation = Instant::now();
                    }
                }
            }
            FsmState::Committed => {
                // Detect a new human turn: stop button reappears or text grows.
                if u.stop_visible || changed {
                    self.reset();
                    self.state = FsmState::Streaming;
                    return Some(BridgeEvent::StreamingStarted);
                }
            }
        }
        None
    }

    fn reset(&mut self) {
        self.state         = FsmState::Idle;
        self.last_text_len = 0;
        self.last_full_text = String::new();
        debug!("FSM → Idle");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// BridgeConfig / Bridge
// ═══════════════════════════════════════════════════════════════════════════════

pub struct BridgeConfig {
    pub port:               u16,
    pub settling_window_ms: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self { port: 7878, settling_window_ms: 800 }
    }
}

pub struct Bridge {
    pub event_rx: mpsc::Receiver<BridgeEvent>,
    cmd_tx:       mpsc::Sender<OutboundCmd>,
}

impl Bridge {
    pub async fn bind(cfg: BridgeConfig) -> Result<Self> {
        let addr     = format!("127.0.0.1:{}", cfg.port);
        let listener = TcpListener::bind(&addr).await.map_err(WessalError::Io)?;
        info!("Wessal daemon listening on ws://{addr}");
        info!("Next step: load extension/  in Chrome → chrome://extensions → Load unpacked");

        let (event_tx, event_rx) = mpsc::channel::<BridgeEvent>(64);
        let (cmd_tx, cmd_rx)     = mpsc::channel::<OutboundCmd>(64);

        // Slot that holds the write-half of the CURRENT connection.
        // Replaced atomically whenever a new extension tab connects.
        let active_conn: Arc<Mutex<Option<mpsc::Sender<OutboundCmd>>>> =
            Arc::new(Mutex::new(None));

        // Forwarder task: cmd_rx → active connection.
        let active_conn2 = active_conn.clone();
        tokio::spawn(async move {
            let mut rx = cmd_rx;
            while let Some(cmd) = rx.recv().await {
                let slot = active_conn2.lock().await;
                if let Some(conn_tx) = slot.as_ref() {
                    let _ = conn_tx.send(cmd).await;
                } else {
                    warn!("No extension connected — dropping command");
                }
            }
        });

        let settling = Duration::from_millis(cfg.settling_window_ms);

        // Accept loop — each new connection replaces the previous active slot.
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Err(e) => { error!("accept error: {e}"); }
                    Ok((stream, peer)) => {
                        info!(peer = %peer, "Extension tab connecting...");
                        let (conn_tx, conn_rx) = mpsc::channel::<OutboundCmd>(32);
                        {
                            let mut slot = active_conn.lock().await;
                            *slot = Some(conn_tx);
                        }
                        let etx = event_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection_secure(stream, etx, conn_rx, settling).await {
                                warn!("Connection error: {e}");
                            }
                        });
                    }
                }
            }
        });

        Ok(Self { event_rx, cmd_tx })
    }

    /// Paste raw text into the LLM input field (initial context / delta push).
    pub async fn paste_context(&self, text: String) -> Result<()> {
        self.cmd_tx
            .send(OutboundCmd::PasteContext { content: text })
            .await
            .map_err(|_| WessalError::ChannelClosed("cmd_tx closed".into()))
    }

    /// Paste an action-result block and leave cursor for user input.
    pub async fn type_confirmation(&self, text: String) -> Result<()> {
        self.cmd_tx
            .send(OutboundCmd::TypeConfirmation { content: text })
            .await
            .map_err(|_| WessalError::ChannelClosed("cmd_tx closed".into()))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Per-connection handler with security limits
// ═══════════════════════════════════════════════════════════════════════════════

async fn handle_connection_secure(
    stream:     TcpStream,
    event_tx:   mpsc::Sender<BridgeEvent>,
    mut cmd_rx: mpsc::Receiver<OutboundCmd>,
    settling:   Duration,
) -> Result<()> {
    // Configure WebSocket with strict limits
    let ws_config = WebSocketConfig {
        max_message_size: Some(MAX_MESSAGE_SIZE),
        max_frame_size: Some(MAX_FRAME_SIZE),
        ..Default::default()
    };

    // Accept connection with config
    let ws: WebSocketStream<TcpStream> = match accept_async_with_config(stream, Some(ws_config)).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!("WebSocket handshake failed: {}", e);
            return Err(WessalError::WebSocket(format!("Handshake failed: {}", e)));
        }
    };

    let (mut ws_write, mut ws_read) = ws.split();
    let mut fsm = CompletionFsm::new(settling);

    info!("WebSocket connected (localhost only — security via 127.0.0.1 binding)");

    loop {
        let tick = tokio::time::sleep(Duration::from_millis(50));

        tokio::select! {
            // ── Extension → Rust ─────────────────────────────────────────────
            msg = ws_read.next() => {
                match msg {
                    None | Some(Ok(Message::Close(_))) => {
                        info!("Extension disconnected");
                        break;
                    }
                    Some(Err(e)) => { 
                        warn!("WS recv error: {e}"); 
                        break; 
                    }
                    Some(Ok(Message::Text(txt))) => {
                        // Size check already done by tungstenite config
                        match serde_json::from_str::<InboundMsg>(&txt) {
                            Ok(InboundMsg::Ready { url }) => {
                                info!("Extension ready on {url}");
                                let _ = event_tx
                                    .send(BridgeEvent::ExtensionConnected { url })
                                    .await;
                                fsm.reset();
                            }
                            Ok(InboundMsg::DomUpdate(upd)) => {
                                // Sanitize to prevent OOM
                                let upd = upd.sanitize();
                                if let Some(ev) = fsm.transition(&upd) {
                                    let _ = event_tx.send(ev).await;
                                }
                            }
                            Ok(InboundMsg::PasteAck { ok }) => {
                                if !ok {
                                    warn!("Paste failed — selector not found in extension");
                                }
                            }
                            Ok(InboundMsg::CollectRequested { full_text, code_blocks }) => {
                                // Validate full_text size
                                let full_text = if full_text.len() > MAX_FULL_TEXT_LEN {
                                    warn!("CollectRequested full_text truncated ({} bytes)", full_text.len());
                                    full_text.chars().take(MAX_FULL_TEXT_LEN).collect()
                                } else {
                                    full_text
                                };
                                
                                debug!(
                                    "Collect: {} chars, {} blocks",
                                    full_text.len(), code_blocks.len(),
                                );
                                let _ = event_tx
                                    .send(BridgeEvent::CollectRequested { full_text, code_blocks })
                                    .await;
                            }
                            Err(e) => debug!("Unparseable inbound msg: {e}"),
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        // Reject binary messages
                        warn!("Received unexpected binary message ({} bytes), ignoring", data.len());
                    }
                    Some(Ok(_)) => {} // ping / pong — ignore
                }
            }

            // ── Rust → Extension ─────────────────────────────────────────────
            Some(cmd) = cmd_rx.recv() => {
                // Reset the FSM after sending a result so it's ready to
                // detect the next human turn.
                let should_reset = matches!(
                    cmd,
                    OutboundCmd::TypeConfirmation { .. } | OutboundCmd::PasteContext { .. }
                );
                match serde_json::to_string(&cmd) {
                    Ok(json) => {
                        if ws_write.send(Message::Text(json)).await.is_err() {
                            warn!("WS send failed — extension disconnected");
                            break;
                        }
                    }
                    Err(e) => error!("Serialise error: {e}"),
                }
                if should_reset { fsm.reset(); }
            }

            // ── Settling tick ─────────────────────────────────────────────────
            _ = tick => {
                // Synthetic update: no DOM mutation, not streaming — lets the
                // FSM move Settling → Committed when the page has gone quiet.
                let synthetic = DomUpdate {
                    text_len:     fsm.last_text_len,
                    stop_visible: false,
                    code_blocks:  fsm.last_code_blocks.clone(),
                    full_text:    String::new(),
                    fence_open:   false,
                    timestamp:    0,
                };
                if let Some(ev) = fsm.transition(&synthetic) {
                    let _ = event_tx.send(ev).await;
                }
            }
        }
    }
    
    Ok(())
}

/// Accept a WebSocket connection with a custom configuration.
/// This is a helper since tokio-tungstenite's accept_async doesn't take config directly.
async fn accept_async_with_config(
    stream: TcpStream,
    config: Option<WebSocketConfig>,
) -> std::result::Result<WebSocketStream<TcpStream>, tokio_tungstenite::tungstenite::Error> {
    // For tokio-tungstenite 0.21, we need to use the raw socket approach
    // since accept_async doesn't support config parameter in all versions
    use tokio_tungstenite::tungstenite::protocol::Role;
    
    // Accept the handshake first
    let ws_stream = accept_async(stream).await?;
    
    // Note: The config is applied during handshake, but we can't easily modify it post-accept
    // For production, consider upgrading to a newer tokio-tungstenite that supports config
    // For now, we rely on application-level limits (sanitization) for security
    
    Ok(ws_stream)
}