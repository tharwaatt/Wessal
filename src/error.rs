// src/error.rs
//
// Fix applied
// ───────────
// • Renamed DevMateError → WessalError throughout.
// • `thiserror` must be in Cargo.toml (now it is).
//   `#[derive(Error)]` generates `impl std::error::Error` automatically,
//   which satisfies the anyhow `?` operator requirement in main.rs.
// • `#[from]` on Io and Json variants generates `impl From<std::io::Error>`
//   and `impl From<serde_json::Error>` so those errors can be converted with `?`.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum WessalError {
    // ── I/O ──────────────────────────────────────────────────────────────────
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // ── Browser / CDP ────────────────────────────────────────────────────────
    #[error("CDP browser error: {0}")]
    Browser(String),

    // ── WebSocket bridge ─────────────────────────────────────────────────────
    #[error("WebSocket bridge error: {0}")]
    WebSocket(String),

    // ── JSON ─────────────────────────────────────────────────────────────────
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    // ── Patch / merge ────────────────────────────────────────────────────────
    #[error("Patch conflict in '{path}': {reason}")]
    PatchConflict { path: String, reason: String },

    // ── Security ─────────────────────────────────────────────────────────────
    #[error("Security violation — path escape: '{attempted}' is outside project root")]
    PathEscape { attempted: String },

    // ── Context ──────────────────────────────────────────────────────────────
    #[error("Context manager error: {0}")]
    Context(String),

    // ── Sandbox ──────────────────────────────────────────────────────────────
    #[error("Sandbox initialisation failed: {0}")]
    Sandbox(String),

    // ── Internal channels ────────────────────────────────────────────────────
    #[error("Internal channel closed: {0}")]
    ChannelClosed(String),
}

/// Convenience alias used in every module: `Result<T>` → `Result<T, WessalError>`.
pub type Result<T> = std::result::Result<T, WessalError>;
