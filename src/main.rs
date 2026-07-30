// src/main.rs — Wessal daemon
//
// Architecture v6.3
// ─────────────────
//   • Event-driven architecture with broadcast channel
//   • Arc<RwLock<AppState>> for shared state
//   • No more SSD thrashing — scan only when necessary
//   • Modular async handlers for each event type
//   • TUI runs in separate tokio task (subscribes to broadcast)
//
// Core principle: read files, write files.
// Human-in-the-loop: the user clicks ⚡ in the browser to trigger execution.

pub mod error;
pub mod context;
pub mod bridge;
pub mod executor;
pub mod chunker;
pub mod actions;
pub mod tui;
pub mod init_payload;

use std::{
    collections::HashMap,
    fs::File,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, error, info, warn};

use context::ProjectMap;
use bridge::{Bridge, BridgeConfig, BridgeEvent};
use executor::{FileExecutor, WriteAction, PatchStatus};
use chunker::{build_packet, PushMode};
use actions::{parse_all_actions, Action};
use tui::{TuiEvent, ActionSummary, ActionKind, ActionStatus};

// ═══════════════════════════════════════════════════════════════════════════════
// System events for broadcast channel
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub enum SystemEvent {
    ExtensionConnected { url: String },
    ExtensionDisconnected,
    ContextPushed { tokens: usize, files: usize },
    ActionExecuted { summary: ActionSummary },
    FileCount { count: usize },
    SessionId { id: String },
    Shutdown,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Shared application state
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub struct AppState {
    pub session_id:     String,
    pub active_url:     Option<String>,
    pub context_pushed: bool,
    pub last_pushed:    ProjectMap,
    pub project_root:   PathBuf,
}

impl AppState {
    fn new(session_id: String, project_root: PathBuf, initial_map: ProjectMap) -> Self {
        Self {
            session_id,
            active_url: None,
            context_pushed: false,
            last_pushed: initial_map,
            project_root,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Resolve project root FIRST so we can place the log file there.
    let project_root = PathBuf::from(
        std::env::args().nth(1).unwrap_or_else(|| ".".into())
    ).canonicalize()?;

    // 2. Route tracing logs to a file to prevent TUI overlap/corruption.
    let log_file = File::create(project_root.join("wessal.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "wessal=debug".into()))
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false) // Disable ANSI color codes in file logs
        .init();

    info!("Wessal v6.3 starting");
    info!(root = %project_root.display());

    // 3. Initial project scan (only once at startup)
    //    Store this as the initial last_pushed state
    let mut initial_map = ProjectMap::new(project_root.clone()).with_load_content(true);
    initial_map.scan().await?;
    let file_count = initial_map.entries().len();

    // 4. Initialize shared state with Arc<RwLock<>>
    let session_id = format!("{:x}", now_secs());
    let state = Arc::new(RwLock::new(AppState::new(
        session_id.clone(),
        project_root.clone(),
        initial_map, // Use the scanned map as initial state
    )));

    // 5. Create broadcast channel for system events
    let (event_tx, _): (broadcast::Sender<SystemEvent>, _) = broadcast::channel(100);

    // 6. Initialize executor
    let executor = FileExecutor::new(project_root.clone())?;

    // 7. Initialize bridge
    let mut bridge = Bridge::bind(BridgeConfig::default()).await?;

    info!("────────────────────────────────────────────────");
    info!("1. chrome://extensions → Load unpacked → extension/");
    info!("2. Open ChatGPT / Gemini / Claude.ai / AI Studio / Kimi / ChatGLM");
    info!("3. Click ⚡ in the page when you see [WESSAL:…] tags");
    info!("────────────────────────────────────────────────");

    // 8. Launch TUI task that subscribes to broadcast events
    let _tui_handle = launch_tui_task(event_tx.subscribe());

    // Send initial events
    let _ = event_tx.send(SystemEvent::SessionId { id: session_id.clone() });
    let _ = event_tx.send(SystemEvent::FileCount { count: file_count });

    // 9. Main event loop
    loop {
        // Wait for bridge events (no more busy-polling / constant scanning!)
        let Some(event) = bridge.event_rx.recv().await else {
            error!("Bridge channel closed");
            break;
        };

        match event {
            BridgeEvent::ExtensionConnected { url } => {
                handle_extension_connected(
                    &state, &bridge, &event_tx, url.clone()
                ).await;
            }

            BridgeEvent::StreamingStarted => {
                // Optional: could trigger delta push here if needed
                debug!("StreamingStarted — waiting for user action");
            }

            BridgeEvent::ResponseCommitted { .. } => {
                debug!("ResponseCommitted — waiting for user to press ⚡ Collect");
            }

            BridgeEvent::CollectRequested { full_text, code_blocks } => {
                handle_collect_requested(
                    &state, &executor, &bridge, &event_tx, &project_root,
                    full_text, code_blocks
                ).await;
            }
        }
    }

    // Cleanup
    let _ = event_tx.send(SystemEvent::Shutdown);
    info!("Shutting down...");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Event handlers
// ═══════════════════════════════════════════════════════════════════════════════

async fn handle_extension_connected(
    state:        &Arc<RwLock<AppState>>,
    bridge:       &Bridge,
    event_tx:     &broadcast::Sender<SystemEvent>,
    url:          String,
) {
    let mut st = state.write().await;
    
    // Send TUI event via broadcast
    let _ = event_tx.send(SystemEvent::ExtensionConnected { 
        url: url.clone() 
    });

    if st.active_url.as_ref() == Some(&url) {
        return;
    }

    info!("Extension connected on {url}");
    st.active_url = Some(url.clone());
    st.context_pushed = true;

    // Send file count to TUI so it populates the stats
    let _ = event_tx.send(SystemEvent::FileCount { count: st.last_pushed.entries().len() });
}

async fn handle_collect_requested(
    state:        &Arc<RwLock<AppState>>,
    executor:     &FileExecutor,
    bridge:       &Bridge,
    event_tx:     &broadcast::Sender<SystemEvent>,
    project_root: &PathBuf,
    full_text:    String,
    code_blocks:  Vec<bridge::RawCodeBlock>,
) {
    info!(
        "Collect: {} chars, {} code block(s)",
        full_text.len(), code_blocks.len(),
    );

    // Extract code from blocks that might contain additional action tags
    let code_extra: String = code_blocks.iter()
        .map(|b| b.code.as_str())
        .filter(|code| {
            (code.contains("[WESSAL:") || code.contains("[DEVMATE:"))
                && !full_text.contains(code.trim())
        })
        .collect::<Vec<_>>()
        .join("\n");

    let scan_text = if code_extra.is_empty() {
        full_text.clone()
    } else {
        format!("{full_text}\n{code_extra}")
    };

    let all_actions = parse_all_actions(&scan_text);

    if all_actions.is_empty() {
        info!("No action tags found in collected text");
        let _ = bridge.paste_context(
            "[WESSAL] No actions found in last message.\n\nUser message: ".into()
        ).await;
        return;
    }

    info!("{} action(s) found", all_actions.len());

    // Scan project ONLY when we need to execute actions
    // This is the ONLY place we scan after startup (no SSD thrashing!)
    let mut fresh = ProjectMap::new(project_root.clone()).with_load_content(true);
    if let Err(e) = fresh.scan().await {
        warn!("Failed to scan project: {e}");
        let _ = bridge.type_confirmation(
            format!("[WESSAL ERROR] Failed to scan project: {e}")
        ).await;
        return;
    }

    let _ = event_tx.send(SystemEvent::FileCount { count: fresh.entries().len() });

    // Build base map for writes (from last pushed state)
    let base_map: HashMap<PathBuf, String> = {
        let st = state.read().await;
        st.last_pushed.entries().iter()
            .filter_map(|(p, e)| e.content.as_ref().map(|c| (p.clone(), c.clone())))
            .collect()
    };

    // Execute actions
    execute_actions(
        all_actions, base_map, &fresh, state, executor, bridge, event_tx, project_root
    ).await;
}

async fn execute_actions(
    actions:     Vec<Action>,
    base_map:    HashMap<PathBuf, String>,
    fresh:       &ProjectMap,
    state:       &Arc<RwLock<AppState>>,
    executor:    &FileExecutor,
    bridge:      &Bridge,
    event_tx:    &broadcast::Sender<SystemEvent>,
    project_root: &PathBuf,
) {
    let is_init_only = actions.len() == 1 && matches!(actions[0], Action::Init);

    let mut read_requests: Vec<(PathBuf, actions::ReadOptions)> = Vec::new();
    let mut exec_results:  Vec<String> = Vec::new();

    for action in actions {
        match action {
            Action::Init => {
                info!("→ init");
                let st = state.read().await;

                let config = crate::init_payload::SmartTreeConfig::default();
                let output = crate::init_payload::build_init_payload(
                    &fresh,
                    &st.project_root,
                    &config,
                );

                let _ = event_tx.send(SystemEvent::ActionExecuted {
                    summary: ActionSummary {
                        time:   now_hms(),
                        kind:   ActionKind::Context,
                        target: "project structure".to_string(),
                        status: ActionStatus::Ok,
                        detail: format!(
                            "{} files, depth ≤ {}",
                            fresh.entries().len(),
                            config.max_depth,
                        ),
                        output: output.clone(),
                    }
                });
                exec_results.push(output);
            }

            Action::Search(keyword) => {
                info!("→ search '{}'", keyword);
                let matches = crate::context::search_project_async(
                    project_root.clone(), keyword.clone()
                ).await;
                
                let output = if matches.is_empty() {
                    format!("=== SEARCH: '{}' ===\nNo matches found.\n", keyword)
                } else {
                    format!("=== SEARCH: '{}' ===\n```text\n{}\n```\n", keyword, matches.join("\n"))
                };

                let _ = event_tx.send(SystemEvent::ActionExecuted { 
                    summary: ActionSummary {
                        time:   now_hms(),
                        kind:   ActionKind::Read,
                        target: format!("search '{}'", keyword),
                        status: ActionStatus::Ok,
                        detail: format!("{} matches", matches.len()),
                        output: output.clone(),
                    }
                });
                exec_results.push(output);
            }

            Action::Outline(path) => {
                info!("→ outline {}", path.display());
                let entry = fresh.get(&path);
                let content = entry.and_then(|e| e.content.as_deref()).unwrap_or("");
                let skeleton = if content.is_empty() {
                    format!("// [WESSAL] File not found or empty: {}", path.display())
                } else {
                    crate::chunker::extract_skeleton(content, &path)
                };

                let lang = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let output = format!(
                    "=== OUTLINE: {} ===\n```{}\n{}\n```\n",
                    path.display(), lang, skeleton.trim_end()
                );

                let _ = event_tx.send(SystemEvent::ActionExecuted { 
                    summary: ActionSummary {
                        time:   now_hms(),
                        kind:   ActionKind::Read,
                        target: format!("outline {}", path.display()),
                        status: ActionStatus::Ok,
                        detail: format!("{} lines", skeleton.lines().count()),
                        output: output.clone(),
                    }
                });
                exec_results.push(output);
            }

            Action::ReadFile { path, options } => {
                info!("→ read {} (L{}–{})", 
                    path.display(), options.offset, options.offset + options.limit - 1);
                
                let _ = event_tx.send(SystemEvent::ActionExecuted { 
                    summary: ActionSummary {
                        time:   now_hms(),
                        kind:   ActionKind::Read,
                        target: path.display().to_string(),
                        status: ActionStatus::Ok,
                        detail: format!("L{}–{}", options.offset, options.offset + options.limit - 1),
                        output: String::new(),
                    }
                });
                read_requests.push((path, options));
            }

            Action::WriteFile { path, content, lang } => {
                info!("→ write {}", path.display());
                let wa = WriteAction { path: path.clone(), content, lang };
                let base = base_map.get(&path).map(|s| s.as_str());
                let r = executor.apply_action(&wa, base).await;

                let msg = match &r.status {
                    PatchStatus::Applied { unified_diff } => {
                        let added = unified_diff.lines()
                            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                            .count();
                        let removed = unified_diff.lines()
                            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                            .count();
                        format!("✓ UPDATED: {}\n  +{added} -{removed} lines (verified from disk)", path.display())
                    }
                    PatchStatus::Created { .. } =>
                        format!("✓ CREATED: {} (verified from disk)", path.display()),
                    PatchStatus::Conflict(_) =>
                        format!("✗ CONFLICT: {} — resolve markers", path.display()),
                    PatchStatus::Error(e) =>
                        format!("✗ ERROR: {}: {e}", path.display()),
                };

                info!("  {msg}");

                let tui_status = if matches!(r.status, PatchStatus::Error(_)) {
                    ActionStatus::Error
                } else {
                    ActionStatus::Ok
                };

                let diff_output = match &r.status {
                    PatchStatus::Applied { unified_diff } => unified_diff.clone(),
                    PatchStatus::Created { verified_diff } => verified_diff.clone(),
                    _ => String::new(),
                };

                let _ = event_tx.send(SystemEvent::ActionExecuted { 
                    summary: ActionSummary {
                        time:   now_hms(),
                        kind:   ActionKind::Write,
                        target: path.display().to_string(),
                        status: tui_status,
                        detail: msg.lines().nth(1).unwrap_or("").trim().to_string(),
                        output: diff_output,
                    }
                });

                exec_results.push(msg);
            }

            Action::ListDir(path) => {
                info!("→ ls {}", path.display());
                let st = state.read().await;
                let listing = list_dir(&path, &st.project_root);
                let entry_count = listing.lines().count().saturating_sub(1);

                let _ = event_tx.send(SystemEvent::ActionExecuted { 
                    summary: ActionSummary {
                        time:   now_hms(),
                        kind:   ActionKind::List,
                        target: path.display().to_string(),
                        status: ActionStatus::Ok,
                        detail: format!("{entry_count} entries"),
                        output: listing.clone(),
                    }
                });
                exec_results.push(listing);
            }
        }
    }

    // Handle read requests in batch
    if !read_requests.is_empty() {
        let st = state.read().await;
        let mut combined = format!(
            "[WESSAL:ON-DEMAND | session:{} | reading {} file(s)]\n\n",
            st.session_id, read_requests.len()
        );
        let mut total_tokens = 0usize;

        for (path, options) in &read_requests {
            let packet = build_packet(
                fresh, &st.last_pushed,
                PushMode::OnDemand(path.clone()),
                st.active_url.as_deref().unwrap_or(""),
                &st.session_id,
            );
            let body = if options.offset == 1 && options.limit == 300 {
                let content = packet.prompt.clone();
                content.find("\n\n").map(|i| content[i + 2..].to_string()).unwrap_or(content)
            } else {
                slice_file_content(fresh, path, options)
            };
            combined.push_str(&body);
            total_tokens += packet.token_estimate;
        }
        combined.push_str("[END WESSAL ON-DEMAND]\n\nUser message: ");
        info!("Sending {} file(s) in one packet (~{total_tokens} tokens)", read_requests.len());

        if let Err(e) = bridge.paste_context(combined).await {
            warn!("File packet failed: {e}");
            exec_results.push(format!("✗ Failed to send {} file(s): {e}", read_requests.len()));
        }
    }

    // Send confirmation for non-read actions
    if !exec_results.is_empty() {
        let msg = if is_init_only { 
            exec_results[0].clone() 
        } else { 
            build_confirmation(&exec_results) 
        };
        if let Err(e) = bridge.type_confirmation(msg).await {
            error!("Confirmation send failed: {e}");
        }
    }

    // Update last_pushed state with the fresh scan
    // This ensures subsequent operations see the current disk state
    let mut st = state.write().await;
    st.last_pushed = fresh.clone();
}

// ═══════════════════════════════════════════════════════════════════════════════
// TUI task launcher — forwards broadcast events to TUI
// ═══════════════════════════════════════════════════════════════════════════════

fn launch_tui_task(mut event_rx: broadcast::Receiver<SystemEvent>) -> tui::TuiHandle {
    let handle = tui::launch();
    let handle_clone = handle.clone();

    // This task runs in a tokio async context and forwards events to the TUI
    // The TUI itself runs in a std::thread (blocking) and receives via std::sync::mpsc
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(SystemEvent::ExtensionConnected { url }) => {
                    handle_clone.send(TuiEvent::Connected {
                        url: url.clone(),
                        llm: llm_from_url(&url).to_string(),
                    });
                }
                Ok(SystemEvent::ExtensionDisconnected) => {
                    handle_clone.send(TuiEvent::Disconnected);
                }
                Ok(SystemEvent::ContextPushed { tokens, files }) => {
                    handle_clone.send(TuiEvent::ContextPushed { tokens, files });
                }
                Ok(SystemEvent::ActionExecuted { summary }) => {
                    handle_clone.send(TuiEvent::ActionExecuted { summary });
                }
                Ok(SystemEvent::FileCount { count }) => {
                    handle_clone.send(TuiEvent::FileCount { count });
                }
                Ok(SystemEvent::SessionId { id }) => {
                    handle_clone.send(TuiEvent::SessionId { id });
                }
                Ok(SystemEvent::Shutdown) | Err(broadcast::error::RecvError::Closed) => {
                    info!("TUI event listener shutting down");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("TUI event channel lagged by {} messages (system under load)", n);
                }
            }
        }
    });

    handle
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper functions
// ═══════════════════════════════════════════════════════════════════════════════

fn slice_file_content(
    map: &ProjectMap, 
    path: &PathBuf, 
    opts: &actions::ReadOptions
) -> String {
    let entry = match map.get(path) {
        Some(e) => e,
        None => return format!("// [WESSAL] File not found: {}\n", path.display()),
    };
    let content = entry.content.as_deref().unwrap_or("");
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = (opts.offset - 1).min(total);
    let end = (start + opts.limit).min(total);
    let lang = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let sliced = lines[start..end].join("\n");
    format!(
        "=== FILE: {} (L{}–L{} of {total}) ===\n```{lang}\n{sliced}\n```\n\n",
        path.display(), opts.offset, end
    )
}

fn list_dir(path: &std::path::Path, root: &std::path::Path) -> String {
    let target = if path.is_absolute() { 
        path.to_path_buf() 
    } else { 
        root.join(path) 
    };
    match std::fs::read_dir(&target) {
        Err(e) => format!("✗ ls {}: {e}", path.display()),
        Ok(entries) => {
            let mut items: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.path().is_dir() { format!("{name}/") } else { name }
                })
                .collect();
            items.sort();
            format!("{}:\n{}", path.display(), items.join("\n"))
        }
    }
}

fn build_confirmation(results: &[String]) -> String {
    let mut s = String::from("[WESSAL RESULT]\n");
    for r in results { 
        s.push_str(r); 
        s.push('\n'); 
    }
    s.push_str("\n[Changes are live on disk]\n\nUser message: ");
    s
}

/// Map a browser URL to a human-readable LLM name shown in the TUI.
fn llm_from_url(url: &str) -> &'static str {
    if url.contains("aistudio.google.com") { "AI Studio" }
    else if url.contains("gemini.google.com") { "Gemini" }
    else if url.contains("chatgpt.com") || url.contains("chat.openai.com") { "ChatGPT" }
    else if url.contains("claude.ai") { "Claude" }
    else if url.contains("kimi.com") { "Kimi" }
    else if url.contains("chat.z.ai") { "ChatGLM" }
    else { "LLM" }
}

fn now_hms() -> String {
    let s = now_secs();
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}