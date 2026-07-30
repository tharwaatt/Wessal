// src/context.rs
//
// v6.3 — OOM Prevention
// ─────────────────────
// Added global memory limit (MAX_TOTAL_CONTENT_BYTES) to prevent loading
// too much file content into memory. When the limit is reached, subsequent
// files have content: None but retain metadata (hash, size, tokens).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{WessalError, Result};

// ── Token estimation ──────────────────────────────────────────────────────────
const BYTES_PER_TOKEN_INV: f32 = 0.25;
const MAX_LINE_LEN:  usize = 2_000;
const MAX_FILE_BYTES: u64  = 512 * 1024; // 512 KiB per file

/// Maximum total content bytes to load into memory (50 MB)
/// Beyond this, files are tracked by metadata only (no content loaded).
const MAX_TOTAL_CONTENT_BYTES: u64 = 50 * 1024 * 1024;

pub type ContentHash = String;
pub fn sha256_of(bytes: &[u8]) -> ContentHash { hex::encode(Sha256::digest(bytes)) }

// ═══════════════════════════════════════════════════════════════════════════════
// FileEntry
// ═══════════════════════════════════════════════════════════════════════════════
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path:           PathBuf,
    pub hash:           ContentHash,
    pub size_bytes:     u64,
    pub modified_secs:  u64,
    pub token_estimate: usize,
    pub content:        Option<String>,
}

impl FileEntry {
    pub fn token_cost(&self) -> usize { self.token_estimate }
}

// ═══════════════════════════════════════════════════════════════════════════════
// FileDelta
// ═══════════════════════════════════════════════════════════════════════════════
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDelta {
    pub path:         PathBuf,
    pub kind:         DeltaKind,
    pub unified_diff: Option<String>,
    pub old_hash:     Option<ContentHash>,
    pub new_hash:     Option<ContentHash>,
    pub token_cost:   usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeltaKind { Added, Modified, Deleted, Renamed { from: PathBuf } }

// ═══════════════════════════════════════════════════════════════════════════════
// ProjectMap
// ═══════════════════════════════════════════════════════════════════════════════
#[derive(Debug, Clone)]
pub struct ProjectMap {
    pub root:   PathBuf,
    entries:    HashMap<PathBuf, FileEntry>,
    load_content: bool,
    /// Total bytes of content currently loaded
    total_content_bytes: u64,
}

impl ProjectMap {
    const DEFAULT_EXTENSIONS: &'static [&'static str] = &[
        "rs", "toml", "md", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs",
        "go", "java", "c", "cpp", "h", "hpp", "cs", "rb", "sh", "bash",
        "yaml", "yml", "json", "sql", "html", "css", "scss",
        "env", "example", "proto",
        "txt", "ini", "cfg", "conf", "config", "lock",
        "dockerfile", "makefile", "mk", "rake",
        "gitignore", "gitattributes", "editorconfig",
        "tf", "hcl", 
    ];

    pub fn new(root: PathBuf) -> Self {
        Self { 
            root, 
            entries: HashMap::new(), 
            load_content: false,
            total_content_bytes: 0,
        }
    }

    pub fn with_load_content(mut self, yes: bool) -> Self {
        self.load_content = yes;
        self
    }

    pub async fn scan(&mut self) -> Result<()> {
        let root         = self.root.clone();
        let load_content = self.load_content;

        let entries = tokio::task::spawn_blocking(move || {
            scan_blocking(&root, Self::DEFAULT_EXTENSIONS, load_content, MAX_FILE_BYTES)
        }).await.map_err(|e| WessalError::Context(format!("scan task panicked: {e}")))?;

        self.entries = entries?;
        
        // Calculate total content bytes
        self.total_content_bytes = self.entries.values()
            .filter_map(|e| e.content.as_ref().map(|c| c.len() as u64))
            .sum();
        
        info!(
            count = self.entries.len(), 
            root = %self.root.display(),
            total_mb = self.total_content_bytes as f64 / (1024.0 * 1024.0),
            "project scan complete"
        );
        Ok(())
    }

    pub fn get(&self, path: &Path) -> Option<&FileEntry> { self.entries.get(path) }
    pub fn entries(&self) -> &HashMap<PathBuf, FileEntry> { &self.entries }
    
    /// Get mutable access to an entry (for on-demand content loading)
    pub fn get_mut(&mut self, path: &Path) -> Option<&mut FileEntry> { 
        self.entries.get_mut(path) 
    }
    
    /// Total content bytes loaded
    pub fn total_content_bytes(&self) -> u64 {
        self.total_content_bytes
    }
    
    /// Load content for a specific file on demand (respects memory limits)
    pub fn load_file_content(&mut self, path: &Path) -> Option<&str> {
        let entry = self.entries.get_mut(path)?;
        
        // Already loaded
        if entry.content.is_some() {
            return entry.content.as_deref();
        }
        
        // Check memory limit
        let prospective_total = self.total_content_bytes + entry.size_bytes;
        if prospective_total > MAX_TOTAL_CONTENT_BYTES {
            warn!(
                "Skipping on-demand load of {} (would exceed memory limit: {} MB)",
                path.display(),
                MAX_TOTAL_CONTENT_BYTES / (1024 * 1024)
            );
            return None;
        }
        
        // Load from disk
        let abs_path = self.root.join(path);
        match std::fs::read_to_string(&abs_path) {
            Ok(content) => {
                self.total_content_bytes += content.len() as u64;
                entry.content = Some(content);
                entry.content.as_deref()
            }
            Err(e) => {
                warn!("Failed to load content for {}: {}", path.display(), e);
                None
            }
        }
    }
}

fn scan_blocking(
    root:         &Path,
    extensions:   &[&str],
    load_content: bool,
    max_bytes:    u64,
) -> Result<HashMap<PathBuf, FileEntry>> {
    let mut entries = HashMap::new();
    let mut total_loaded: u64 = 0;

    for result in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .add_custom_ignore_filename(".wessalignore") // Robust ignore support
        .build()
    {
        let dir_entry: ignore::DirEntry = match result {
            Ok(e)  => e,
            Err(_) => continue,
        };

        let path = dir_entry.path();
        if !path.is_file() { continue; }

        let ext = path.extension().and_then(|e: &std::ffi::OsStr| e.to_str()).unwrap_or("");
        if !extensions.iter().any(|&x| x.eq_ignore_ascii_case(ext)) { continue; }

        let meta: std::fs::Metadata = match dir_entry.metadata() {
            Ok(m)  => m,
            Err(_) => continue,
        };
        if meta.len() > max_bytes { 
            debug!("Skipping large file: {} ({} bytes)", path.display(), meta.len());
            continue;
        }

        let raw = match std::fs::read(path) {
            Ok(b)  => b,
            Err(_) => continue,
        };
        
        // Try to parse as UTF-8
        let text_result = String::from_utf8(raw.clone());
        let text = match text_result {
            Ok(s)  => s,
            Err(_) => {
                // Binary file — track metadata only, no content
                let hash = sha256_of(&raw);
                let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
                let modified_secs: u64 = meta.modified().ok()
                    .and_then(|t: SystemTime| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d: Duration| d.as_secs())
                    .unwrap_or(0);
                
                entries.insert(rel_path.clone(), FileEntry {
                    path: rel_path,
                    hash,
                    size_bytes: meta.len(),
                    modified_secs,
                    token_estimate: 0, // Binary files have no tokens
                    content: None,
                });
                continue;
            }
        };
        
        if text.lines().any(|l| l.len() > MAX_LINE_LEN) { 
            debug!("Skipping file with long lines: {}", path.display());
            continue; 
        }

        let hash            = sha256_of(&raw);
        let token_estimate  = (text.len() as f32 * BYTES_PER_TOKEN_INV) as usize;

        let modified_secs: u64 = meta.modified().ok()
            .and_then(|t: SystemTime| t.duration_since(UNIX_EPOCH).ok())
            .map(|d: Duration| d.as_secs())
            .unwrap_or(0);
        
        let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();

        // OOM prevention: check total loaded content
        let should_load_content = if load_content {
            let prospective = total_loaded + text.len() as u64;
            if prospective > MAX_TOTAL_CONTENT_BYTES {
                warn!(
                    "Memory limit reached ({} MB) — skipping content for {}",
                    MAX_TOTAL_CONTENT_BYTES / (1024 * 1024),
                    rel_path.display()
                );
                false
            } else {
                total_loaded += text.len() as u64;
                true
            }
        } else {
            false
        };

        entries.insert(rel_path.clone(), FileEntry {
            path: rel_path, 
            hash, 
            size_bytes: meta.len(), 
            modified_secs, 
            token_estimate, 
            content: if should_load_content { Some(text) } else { None },
        });
    }
    
    info!(
        "Scan complete: {} files, {:.2} MB content loaded",
        entries.len(),
        total_loaded as f64 / (1024.0 * 1024.0)
    );
    
    Ok(entries)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Search — fast, blocking, respects .gitignore and .wessalignore
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn search_project_async(root: PathBuf, keyword: String) -> Vec<String> {
    tokio::task::spawn_blocking(move || {
        let mut results = Vec::new();
        let mut builder = WalkBuilder::new(&root);
        builder.hidden(false)
               .git_ignore(true)
               .git_global(true)
               .git_exclude(true)
               .require_git(false)
               .add_custom_ignore_filename(".wessalignore");

        let kw = keyword.to_lowercase();
        let mut match_count = 0;

        for result in builder.build() {
            if match_count >= 50 { break; }
            let dir_entry = match result { Ok(e) => e, Err(_) => continue };
            if !dir_entry.path().is_file() { continue; }
            
            if let Ok(text) = std::fs::read_to_string(dir_entry.path()) {
                for (i, line) in text.lines().enumerate() {
                    if line.to_lowercase().contains(&kw) {
                        let rel_path = dir_entry.path().strip_prefix(&root).unwrap_or(dir_entry.path());
                        let formatted = format!("{}:{}: {}", rel_path.display(), i + 1, line.trim());
                        
                        // Prevent massive minified lines from exploding the terminal/chat window
                        // UTF-8 safe truncation using chars
                        let truncated: String = formatted.chars().take(200).collect();
                        results.push(truncated);
                        
                        match_count += 1;
                        if match_count >= 50 { break; }
                    }
                }
            }
        }
        results
    }).await.unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════════════
// compute_delta and diffing
// ═══════════════════════════════════════════════════════════════════════════════

pub fn compute_delta(previous: &ProjectMap, current: &ProjectMap) -> Vec<FileDelta> {
    let mut deltas = Vec::new();

    for (path, prev_entry) in previous.entries() {
        match current.entries().get(path) {
            None => {
                deltas.push(FileDelta { 
                    path: path.clone(), 
                    kind: DeltaKind::Deleted, 
                    unified_diff: None, 
                    old_hash: Some(prev_entry.hash.clone()), 
                    new_hash: None, 
                    token_cost: 20 
                });
            }
            Some(cur_entry) if cur_entry.hash != prev_entry.hash => {
                let old_text = prev_entry.content.as_deref().unwrap_or("");
                let new_text = cur_entry.content.as_deref().unwrap_or("");
                let diff_str = make_unified_diff(path, old_text, new_text, 3);
                let token_cost = (diff_str.len() as f32 * BYTES_PER_TOKEN_INV) as usize;
                deltas.push(FileDelta { 
                    path: path.clone(), 
                    kind: DeltaKind::Modified, 
                    unified_diff: Some(diff_str), 
                    old_hash: Some(prev_entry.hash.clone()), 
                    new_hash: Some(cur_entry.hash.clone()), 
                    token_cost 
                });
            }
            _ => {}
        }
    }

    for (path, cur_entry) in current.entries() {
        if previous.entries().contains_key(path) { continue; }
        deltas.push(FileDelta { 
            path: path.clone(), 
            kind: DeltaKind::Added, 
            unified_diff: None, 
            old_hash: None, 
            new_hash: Some(cur_entry.hash.clone()), 
            token_cost: cur_entry.token_estimate 
        });
    }

    deltas.sort_by_key(|d| match &d.kind { 
        DeltaKind::Modified => 0u8, 
        DeltaKind::Added => 1, 
        DeltaKind::Renamed { .. } => 1, 
        DeltaKind::Deleted => 2 
    });
    deltas
}

pub fn make_unified_diff(path: &Path, old: &str, new: &str, ctx: usize) -> String {
    let diff    = TextDiff::from_lines(old, new);
    let display = path.display().to_string();
    let mut out = format!("--- a/{display}\n+++ b/{display}\n");

    for group in diff.grouped_ops(ctx) {
        let old_start: usize = group.first().map(|op| op.old_range().start + 1).unwrap_or(1);
        let new_start: usize = group.first().map(|op| op.new_range().start + 1).unwrap_or(1);
        let old_len:   usize = group.iter().map(|op| op.old_range().len()).sum();
        let new_len:   usize = group.iter().map(|op| op.new_range().len()).sum();
        out.push_str(&format!("@@ -{old_start},{old_len} +{new_start},{new_len} @@\n"));

        for op in &group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() { 
                    ChangeTag::Delete => '-', 
                    ChangeTag::Insert => '+', 
                    ChangeTag::Equal  => ' ' 
                };
                let val = change.value();
                out.push(sign); 
                out.push_str(val);
                if !val.ends_with('\n') { out.push('\n'); }
            }
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════════
// Unit tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_of() {
        let hash = sha256_of(b"hello world");
        assert_eq!(hash.len(), 64); // SHA-256 produces 32 bytes = 64 hex chars
    }

    #[test]
    fn test_token_estimation() {
        // Rough estimation: 4 chars per token
        let text = "hello world test";
        let tokens = (text.len() as f32 * BYTES_PER_TOKEN_INV) as usize;
        assert!(tokens > 0);
    }
}