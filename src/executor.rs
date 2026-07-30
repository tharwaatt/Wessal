// src/executor.rs
//
// v6.3 — Size Limits & Security
// ─────────────────────────────
// Added MAX_WRITE_CONTENT_SIZE to prevent massive file writes from crashing
// the daemon. Content exceeding this limit is rejected with an error.
//
// v2.1 — Disk Verification
// ────────────────────────
// After every successful write_atomic(), the file is re-read from disk and the
// unified diff shown in the TUI is generated from that on-disk content — NOT
// from what the LLM sent.  This gives an absolute guarantee that the diff the
// user reviews matches exactly what was persisted.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tracing::{debug, info, warn};

use crate::error::{WessalError, Result};

// ══════════════════════════════════════════════════════════════════════════════
// Security constants
// ══════════════════════════════════════════════════════════════════════════════

/// Maximum size for write content (5 MB)
/// Prevents OOM from malicious or accidental massive file writes.
const MAX_WRITE_CONTENT_SIZE: usize = 5 * 1024 * 1024;

/// Maximum path length to prevent filesystem issues
const MAX_PATH_LENGTH: usize = 4096;

// ══════════════════════════════════════════════════════════════════════════════
// WriteAction — the unit of work for a file write
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteAction {
    pub path:    PathBuf,
    pub content: String,
    pub lang:    String,
}

impl WriteAction {
    /// Validate the write action before execution
    pub fn validate(&self) -> Result<()> {
        // Check path length
        if self.path.as_os_str().len() > MAX_PATH_LENGTH {
            return Err(WessalError::Context(format!(
                "Path too long ({} > {}): {}",
                self.path.as_os_str().len(),
                MAX_PATH_LENGTH,
                self.path.display()
            )));
        }
        
        // Check content size
        if self.content.len() > MAX_WRITE_CONTENT_SIZE {
            return Err(WessalError::Context(format!(
                "Write content too large ({} bytes > {} MB limit): {}",
                self.content.len(),
                MAX_WRITE_CONTENT_SIZE / (1024 * 1024),
                self.path.display()
            )));
        }
        
        Ok(())
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// PatchResult
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct PatchResult {
    pub path:   PathBuf,
    pub status: PatchStatus,
}

#[derive(Debug, Clone)]
pub enum PatchStatus {
    /// File was modified.  unified_diff is built from what was actually on disk
    /// *after* the write — not from the LLM's text.
    Applied { unified_diff: String },

    /// File did not previously exist and was created.
    /// verified_diff is the diff from an empty baseline to the saved disk content,
    /// so the reviewer can confirm exactly what landed on disk.
    Created { verified_diff: String },

    /// 3-way merge produced conflict markers — human resolution needed.
    Conflict(ConflictData),

    /// I/O or validation error.
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ConflictData {
    pub base:          String,
    pub local:         String,
    pub ai_version:    String,
    pub conflict_text: String,
}

// ══════════════════════════════════════════════════════════════════════════════
// Path validation — the primary security boundary
// ══════════════════════════════════════════════════════════════════════════════

fn validate_path(root: &Path, relative_path: &Path) -> Result<PathBuf> {
    use std::path::Component;

    // Absolute paths are never allowed.
    if relative_path.is_absolute() {
        return Err(WessalError::PathEscape {
            attempted: relative_path.display().to_string(),
        });
    }

    // Reject NUL bytes.
    let s = relative_path.to_string_lossy();
    if s.contains('\0') {
        return Err(WessalError::PathEscape {
            attempted: format!("{s} (contains NUL)"),
        });
    }

    // Reject any ".." traversal attempts.
    for component in relative_path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(WessalError::PathEscape {
                attempted: relative_path.display().to_string(),
            });
        }
    }

    // Reject suspicious patterns
    let path_str = relative_path.to_string_lossy();
    let lower = path_str.to_lowercase();
    
    // Block hidden directory traversal outside project
    if lower.contains("/.git/") || lower.contains("\\.git\\") {
        // Allow .git files in project, but log
        debug!("Write to .git path: {}", relative_path.display());
    }
    
    // Canonicalize only the trusted project root.
    let canon_root = root.canonicalize().map_err(WessalError::Io)?;

    // Preserve the full relative path exactly as requested.
    let abs_path = canon_root.join(relative_path);
    
    // Verify the canonicalized result is still under root
    // (catches symlinks that escape)
    if let Ok(canon_target) = abs_path.canonicalize() {
        if !canon_target.starts_with(&canon_root) {
            return Err(WessalError::PathEscape {
                attempted: format!("{} (resolves outside project)", relative_path.display()),
            });
        }
    }

    Ok(abs_path)
}

// ══════════════════════════════════════════════════════════════════════════════
// Disk verification helper
//
// Reads `path` back from disk after a successful write.  Returns the content
// as a String, or a PatchResult::Error if the read fails.
// ══════════════════════════════════════════════════════════════════════════════

fn read_back(abs_path: &Path, rel_path: &Path) -> std::result::Result<String, PatchResult> {
    fs::read_to_string(abs_path).map_err(|e| PatchResult {
        path:   rel_path.to_path_buf(),
        status: PatchStatus::Error(format!(
            "disk verification read failed for '{}': {e}",
            abs_path.display()
        )),
    })
}

// ══════════════════════════════════════════════════════════════════════════════
// FileExecutor
// ══════════════════════════════════════════════════════════════════════════════

pub struct FileExecutor {
    project_root: PathBuf,
}

impl FileExecutor {
    /// Create a new executor rooted at `project_root`.
    /// Canonicalises the root path; returns an error if it doesn't exist.
    pub fn new(project_root: PathBuf) -> Result<Self> {
        let root = project_root.canonicalize().map_err(WessalError::Io)?;
        info!("FileExecutor: path jail active — all writes restricted to {}", root.display());
        Ok(Self { project_root: root })
    }

    // ── Main dispatch ─────────────────────────────────────────────────────────

    pub async fn apply_action(
        &self,
        action:       &WriteAction,
        base_content: Option<&str>,
    ) -> PatchResult {
        // Validate action first (size limits)
        if let Err(e) = action.validate() {
            return PatchResult {
                path:   action.path.clone(),
                status: PatchStatus::Error(e.to_string()),
            };
        }
        
        let abs_path = match validate_path(&self.project_root, &action.path) {
            Ok(p)  => p,
            Err(e) => return PatchResult {
                path:   action.path.clone(),
                status: PatchStatus::Error(e.to_string()),
            },
        };

        let local_content_opt = match fs::read_to_string(&abs_path) {
            Ok(s)                                                     => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound       => None,
            Err(e) => return PatchResult {
                path:   action.path.clone(),
                status: PatchStatus::Error(format!("read error: {e}")),
            },
        };

        match (base_content, local_content_opt.as_deref()) {

            // ── New file ──────────────────────────────────────────────────────
            (None, None) => {
                match self.write_atomic(&abs_path, &action.content).await {
                    Err(e) => PatchResult {
                        path:   action.path.clone(),
                        status: PatchStatus::Error(e.to_string()),
                    },
                    Ok(()) => {
                        // Disk verification: diff from empty baseline → actual file on disk.
                        match read_back(&abs_path, &action.path) {
                            Err(r)           => r,
                            Ok(disk_content) => {
                                let verified_diff = crate::context::make_unified_diff(
                                    &action.path, "", &disk_content, 3,
                                );
                                info!("Created (verified): {}", action.path.display());
                                PatchResult {
                                    path:   action.path.clone(),
                                    status: PatchStatus::Created { verified_diff },
                                }
                            }
                        }
                    }
                }
            }

            // ── Clean two-way apply (no local changes since context push) ─────
            (Some(base), Some(local)) if base == local => {
                match self.write_atomic(&abs_path, &action.content).await {
                    Err(e) => PatchResult {
                        path:   action.path.clone(),
                        status: PatchStatus::Error(e.to_string()),
                    },
                    Ok(()) => {
                        // Disk verification: diff from base → what was actually saved.
                        match read_back(&abs_path, &action.path) {
                            Err(r)           => r,
                            Ok(disk_content) => {
                                let unified_diff = crate::context::make_unified_diff(
                                    &action.path, base, &disk_content, 3,
                                );
                                info!("Applied (verified): {}", action.path.display());
                                PatchResult {
                                    path:   action.path.clone(),
                                    status: PatchStatus::Applied { unified_diff },
                                }
                            }
                        }
                    }
                }
            }

            // ── Local edits occurred — attempt 3-way merge ───────────────────
            (Some(base), Some(local)) => {
                self.three_way_merge(&action.path, &abs_path, base, local, &action.content).await
            }

            // ── LLM knew the file but it was deleted locally — recreate ───────
            (Some(_base), None) => {
                warn!(
                    "File {} had a base but is missing on disk — recreating",
                    action.path.display()
                );
                match self.write_atomic(&abs_path, &action.content).await {
                    Err(e) => PatchResult {
                        path:   action.path.clone(),
                        status: PatchStatus::Error(e.to_string()),
                    },
                    Ok(()) => {
                        match read_back(&abs_path, &action.path) {
                            Err(r)           => r,
                            Ok(disk_content) => {
                                let verified_diff = crate::context::make_unified_diff(
                                    &action.path, "", &disk_content, 3,
                                );
                                PatchResult {
                                    path:   action.path.clone(),
                                    status: PatchStatus::Created { verified_diff },
                                }
                            }
                        }
                    }
                }
            }

            // ── LLM has no prior context for this file — overwrite with diff ──
            (None, Some(local)) => {
                match self.write_atomic(&abs_path, &action.content).await {
                    Err(e) => PatchResult {
                        path:   action.path.clone(),
                        status: PatchStatus::Error(e.to_string()),
                    },
                    Ok(()) => {
                        // Disk verification: diff from old local content → actual disk.
                        match read_back(&abs_path, &action.path) {
                            Err(r)           => r,
                            Ok(disk_content) => {
                                let unified_diff = crate::context::make_unified_diff(
                                    &action.path, local, &disk_content, 3,
                                );
                                info!("Applied/overwrite (verified): {}", action.path.display());
                                PatchResult {
                                    path:   action.path.clone(),
                                    status: PatchStatus::Applied { unified_diff },
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── 3-way merge using diffy ───────────────────────────────────────────────

    async fn three_way_merge(
        &self,
        rel_path:   &Path,
        abs_path:   &Path,
        base:       &str,
        local:      &str,
        ai_version: &str,
    ) -> PatchResult {
        match diffy::merge(base, local, ai_version) {
            Ok(merged) => {
                let merged: String = merged;
                match self.write_atomic(abs_path, &merged).await {
                    Err(e) => PatchResult {
                        path:   rel_path.to_path_buf(),
                        status: PatchStatus::Error(e.to_string()),
                    },
                    Ok(()) => {
                        // Disk verification: diff from local → what was actually saved.
                        match read_back(abs_path, rel_path) {
                            Err(r)           => r,
                            Ok(disk_content) => {
                                let unified_diff = crate::context::make_unified_diff(
                                    rel_path, local, &disk_content, 3,
                                );
                                info!("3-way merge clean (verified): {}", rel_path.display());
                                PatchResult {
                                    path:   rel_path.to_path_buf(),
                                    status: PatchStatus::Applied { unified_diff },
                                }
                            }
                        }
                    }
                }
            }
            Err(conflict) => {
                let conflict_text: String = conflict.to_string();
                warn!(
                    "3-way merge conflict in {} — staged for human review",
                    rel_path.display()
                );
                PatchResult {
                    path:   rel_path.to_path_buf(),
                    status: PatchStatus::Conflict(ConflictData {
                        base:          base.to_string(),
                        local:         local.to_string(),
                        ai_version:    ai_version.to_string(),
                        conflict_text,
                    }),
                }
            }
        }
    }

    // ── Atomic write via tempfile + rename ────────────────────────────────────

    pub async fn write_atomic(&self, path: &Path, content: &str) -> Result<()> {
        // Final size check
        if content.len() > MAX_WRITE_CONTENT_SIZE {
            return Err(WessalError::Context(format!(
                "Content too large for write: {} bytes (max {} MB)",
                content.len(),
                MAX_WRITE_CONTENT_SIZE / (1024 * 1024)
            )));
        }
        
        // Ensure all parent directories exist.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(WessalError::Io)?;
        }
        let path_buf = path.to_path_buf();
        let content  = content.to_string();

        tokio::task::spawn_blocking(move || {
            let parent = path_buf.parent().ok_or_else(|| WessalError::Io(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"),
            ))?;
            let mut tmp = NamedTempFile::new_in(parent).map_err(WessalError::Io)?;
            std::io::Write::write_all(&mut tmp, content.as_bytes()).map_err(WessalError::Io)?;
            tmp.as_file().sync_all().map_err(WessalError::Io)?;
            // Atomic rename — avoids partial writes being visible.
            tmp.persist(&path_buf).map_err(|e| WessalError::Io(e.error))?;
            debug!("Atomic write: {}", path_buf.display());
            Ok(())
        })
        .await
        .map_err(|e| WessalError::Io(
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
        ))?
    }

    // ── Batch apply (used by main.rs for multi-write responses) ───────────────

    pub async fn apply_all(
        &self,
        actions:  &[WriteAction],
        base_map: &HashMap<PathBuf, String>,
    ) -> Vec<PatchResult> {
        let mut results = Vec::with_capacity(actions.len());
        for action in actions {
            let base   = base_map.get(&action.path).map(|s| s.as_str());
            let result = self.apply_action(action, base).await;
            match &result.status {
                PatchStatus::Applied { .. }      => info!("[OK]       {}", action.path.display()),
                PatchStatus::Created { .. }      => info!("[CREATED]  {}", action.path.display()),
                PatchStatus::Conflict(_)         => warn!("[CONFLICT] {}", action.path.display()),
                PatchStatus::Error(e)            => tracing::error!("[ERROR]    {}: {e}", action.path.display()),
            }
            results.push(result);
        }
        results
    }

    // ── Result message builder (pasted into the chat input) ──────────────────

    pub fn build_confirmation(results: &[PatchResult]) -> String {
        let mut msg = String::from("[WESSAL RESULT]\n");
        for r in results {
            match &r.status {
                PatchStatus::Applied { .. } =>
                    msg.push_str(&format!("✓ UPDATED:  {}\n", r.path.display())),
                PatchStatus::Created { .. } =>
                    msg.push_str(&format!("✓ CREATED:  {}\n", r.path.display())),
                PatchStatus::Conflict(_) =>
                    msg.push_str(&format!(
                        "✗ CONFLICT: {} — resolve conflict markers manually\n",
                        r.path.display()
                    )),
                PatchStatus::Error(e) =>
                    msg.push_str(&format!("✗ ERROR:    {}: {e}\n", r.path.display())),
            }
        }
        msg.push_str("\n[Changes are live on disk]\n\nUser message: ");
        msg
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Unit tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_action_validation_size() {
        let large_content = "x".repeat(MAX_WRITE_CONTENT_SIZE + 1);
        let action = WriteAction {
            path: PathBuf::from("test.txt"),
            content: large_content,
            lang: "txt".to_string(),
        };
        
        assert!(action.validate().is_err());
    }
    
    #[test]
    fn test_write_action_validation_ok() {
        let action = WriteAction {
            path: PathBuf::from("test.txt"),
            content: "hello world".to_string(),
            lang: "txt".to_string(),
        };
        
        assert!(action.validate().is_ok());
    }
    
    #[test]
    fn test_path_validation_absolute() {
        let root = std::env::current_dir().unwrap();
        let result = validate_path(&root, Path::new("/etc/passwd"));
        assert!(result.is_err());
    }
    
    #[test]
    fn test_path_validation_traversal() {
        let root = std::env::current_dir().unwrap();
        let result = validate_path(&root, Path::new("../outside.txt"));
        assert!(result.is_err());
    }
}