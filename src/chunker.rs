// src/chunker.rs
//
// ─── Smart Context Chunker ────────────────────────────────────────────────────
//
// Four-stage pipeline that converts a ProjectMap into a prompt that fits the
// active LLM's input budget without sacrificing usefulness:
//
//   Stage 1 · TargetBudget   — per-LLM token ceiling from the URL
//   Stage 2 · PushMode       — Initial / Delta / OnDemand (what changed?)
//   Stage 3 · FileRanker     — score every file by recency + graph position
//   Stage 4 · ContentShaper  — Full / Skeleton / Diff / Summary per file
//   Stage 5 · BudgetPacker   — greedy fill, renders the final prompt string
//
// Token estimation: 1 token ≈ 4 bytes of UTF-8 source code.
// All estimates are conservative (divide by 3.5 rather than 4) to leave
// headroom for the prompt header and LLM overhead.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::info;

use crate::context::{compute_delta, FileEntry, FileDelta, DeltaKind, ProjectMap};

// ── Token estimation ──────────────────────────────────────────────────────────
pub fn estimate_tokens(s: &str) -> usize {
    // Conservative: ~3.5 bytes/token. Integer math: (len * 2) / 7 ≈ len / 3.5
    (s.len() * 2) / 7
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stage 1 — TargetBudget
// ═══════════════════════════════════════════════════════════════════════════════

/// Per-LLM token budget for the *input* side of the context window.
/// We reserve 20 % for the response, so these are 80 % of the true limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetBudget {
    /// Maximum tokens for the full initial context push.
    pub initial: usize,
    /// Maximum tokens for a delta push (always smaller — just the changes).
    pub delta: usize,
    /// Maximum tokens for a single on-demand file fetch.
    pub on_demand: usize,
    /// When true, use Skeleton mode for unchanged files.
    pub prefer_skeleton: bool,
}

impl TargetBudget {
    /// Infer the budget from the URL hostname the extension is connected to.
    pub fn from_url(url: &str) -> Self {
        if url.contains("claude.ai") {
            Self {
                initial:         160_000,
                delta:            20_000,
                on_demand:        40_000,
                prefer_skeleton: false,  // Claude handles large contexts well
            }
        } else if url.contains("gemini.google.com") {
            Self {
                initial:         128_000,
                delta:            16_000,
                on_demand:        32_000,
                prefer_skeleton: false,
            }
        } else if url.contains("chatgpt.com") || url.contains("chat.openai.com") {
            // ChatGPT web enforces a ~4 000 character paste limit in the input
            // field. We work around it by sending only the project skeleton
            // (function signatures + struct defs) and letting the LLM ask for
            // specific files via [WESSAL:read:path].
            Self {
                initial:           3_000, // ~12 000 chars at 4 bytes/token
                delta:               800,
                on_demand:         2_500,
                prefer_skeleton:  true,
            }
        } else {
            // Unknown / local (Ollama, Open-WebUI, etc.) — generous defaults.
            Self {
                initial:          32_000,
                delta:             8_000,
                on_demand:        16_000,
                prefer_skeleton: false,
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stage 2 — PushMode
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushMode {
    /// First push of the session: send the full project skeleton.
    Initial,
    /// Files changed since last push: send only the deltas.
    Delta,
    /// LLM explicitly requested a specific file via [WESSAL:read:path].
    OnDemand(PathBuf),
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stage 3 — FileRanker
// ═══════════════════════════════════════════════════════════════════════════════

/// Score assigned to each file. Higher = more important = included first.
#[derive(Debug, Clone)]
pub struct RankedFile<'a> {
    pub entry: &'a FileEntry,
    pub score: f64,
    /// Whether this file appears in the delta (recently changed).
    pub is_delta: bool,
}

/// Rank every file in `map` relative to `deltas` and the current time.
pub fn rank_files<'a>(
    map:     &'a ProjectMap,
    deltas:  &[FileDelta],
    now_secs: u64,
) -> Vec<RankedFile<'a>> {
    let delta_paths: std::collections::HashSet<&PathBuf> =
        deltas.iter().map(|d| &d.path).collect();

    let mut ranked: Vec<RankedFile<'a>> = map
        .entries()
        .values()
        .map(|entry| {
            let is_delta = delta_paths.contains(&entry.path);
            let score    = score_file(entry, is_delta, now_secs);
            RankedFile { entry, score, is_delta }
        })
        .collect();

    // Descending: highest-priority files first.
    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    ranked
}

fn score_file(entry: &FileEntry, is_delta: bool, now_secs: u64) -> f64 {
    const DECAY_MINUTES: f64 = 60.0;
    const RECENCY_BOOST: f64 = 200.0;
    const BACKGROUND:    f64 =  10.0;
    const KB_PENALTY:    f64 =   0.8;

    let age_min = if entry.modified_secs > 0 && now_secs > entry.modified_secs {
        (now_secs - entry.modified_secs) as f64 / 60.0
    } else {
        1_440.0 // treat unknown → 24 h old
    };

    let recency = if is_delta {
        RECENCY_BOOST * (-age_min / DECAY_MINUTES).exp()
    } else {
        BACKGROUND * (-age_min / (DECAY_MINUTES * 4.0)).exp()
    };

    // Bonus for entry-point / config files that are always relevant.
    let entrypoint_bonus = if is_entrypoint(&entry.path) { 30.0 } else { 0.0 };

    // Penalty proportional to size: large files cost more tokens.
    let size_penalty = (entry.size_bytes as f64 / 1_024.0) * KB_PENALTY;

    recency + entrypoint_bonus - size_penalty
}

fn is_entrypoint(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(name,
        "main.rs" | "lib.rs" | "mod.rs" |
        "main.py" | "__init__.py" |
        "index.ts" | "index.js" | "app.ts" | "app.js" |
        "Cargo.toml" | "package.json" | "pyproject.toml" |
        "README.md" | "ARCHITECTURE.md"
    )
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stage 4 — ContentShaper
// ═══════════════════════════════════════════════════════════════════════════════

/// How a file's content is represented in the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeKind {
    /// Complete file content.
    Full,
    /// Function/type signatures only — bodies stripped.
    Skeleton,
    /// Only the unified diff hunks (for delta pushes).
    Diff,
    /// One-liner: path, size, hash — LLM knows the file exists.
    Summary,
}

/// A file as it will appear in the rendered prompt.
#[derive(Debug, Clone)]
pub struct ShapedFile {
    pub path:       PathBuf,
    pub kind:       ShapeKind,
    pub content:    String,   // the shaped text
    pub token_cost: usize,
}

/// Choose the best shape for a file given the budget remaining.
pub fn shape_file(
    entry:          &FileEntry,
    delta:          Option<&FileDelta>,  // Some if this file changed
    budget_left:    usize,
    prefer_skeleton: bool,
) -> ShapedFile {
    let full_content = entry.content.as_deref().unwrap_or("");
    let full_cost    = estimate_tokens(full_content);

    // ── Diff shape: only for changed files in Delta mode ─────────────────────
    if let Some(d) = delta {
        if let Some(diff) = &d.unified_diff {
            let cost = estimate_tokens(diff);
            if cost <= budget_left {
                return ShapedFile {
                    path:       entry.path.clone(),
                    kind:       ShapeKind::Diff,
                    content:    diff.clone(),
                    token_cost: cost,
                };
            }
        }
    }

    // ── Full shape ────────────────────────────────────────────────────────────
    if full_cost <= budget_left && !prefer_skeleton {
        return ShapedFile {
            path:       entry.path.clone(),
            kind:       ShapeKind::Full,
            content:    full_content.to_string(),
            token_cost: full_cost,
        };
    }

    // ── Skeleton shape: strip function bodies ─────────────────────────────────
    let skeleton    = extract_skeleton(full_content, &entry.path);
    let skel_cost   = estimate_tokens(&skeleton);

    if skel_cost <= budget_left {
        return ShapedFile {
            path:       entry.path.clone(),
            kind:       ShapeKind::Skeleton,
            content:    skeleton,
            token_cost: skel_cost,
        };
    }

    // ── Summary shape: absolute last resort ───────────────────────────────────
    let summary = format!(
        "// [WESSAL:summary] {} — {} bytes, {} lines (use [WESSAL:read:{}] to fetch)\n",
        entry.path.display(),
        entry.size_bytes,
        full_content.lines().count(),
        entry.path.display(),
    );
    ShapedFile {
        path:       entry.path.clone(),
        kind:       ShapeKind::Summary,
        content:    summary.clone(),
        token_cost: estimate_tokens(&summary),
    }
}

/// Extract only the "public surface" of a source file:
///   - Rust:   fn/struct/enum/trait/impl signatures + doc comments
///   - Python: class/def signatures + docstrings
///   - JS/TS:  export function/class/const declarations
///   - Other:  first 40 lines (reasonable header)
///
/// Bodies are replaced with `{ /* … */ }` so the LLM understands the shape
/// without paying for the full implementation tokens.
pub fn extract_skeleton(content: &str, path: &Path) -> String {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs"              => skeleton_rust(content),
        "py"              => skeleton_python(content),
        "ts" | "tsx" | "js" | "jsx" | "mjs" => skeleton_js(content),
        _                 => skeleton_head(content, 40),
    }
}

// ── Rust skeleton ─────────────────────────────────────────────────────────────
fn skeleton_rust(src: &str) -> String {
    let mut out     = String::new();
    let mut depth   = 0i32;
    let mut in_body = false;
    let mut skipped = 0usize;

    for line in src.lines() {
        let trimmed = line.trim();

        // Always keep: attributes, use, mod, pub items, doc comments, blank lines.
        let _is_signature = trimmed.starts_with("pub ")
            || trimmed.starts_with("#[")
            || trimmed.starts_with("//")
            || trimmed.starts_with("use ")
            || trimmed.starts_with("mod ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("static ")
            || trimmed.is_empty();

        if !in_body {
            out.push_str(line);
            out.push('\n');

            // Entering a body block
            let opens: i32 = line.chars().filter(|&c| c == '{').count() as i32;
            let closes: i32 = line.chars().filter(|&c| c == '}').count() as i32;
            depth += opens - closes;

            // A fn/impl/struct with a '{' opens a body we may want to collapse.
            // Only collapse function bodies (depth == 1 after entering).
            if depth == 1 && opens > 0
                && (trimmed.contains("fn ")
                    || trimmed.starts_with("impl")
                    || trimmed.starts_with("async fn"))
                && !trimmed.ends_with(';')
            {
                // If the body is all on one line, keep it.
                if closes > 0 && depth == 0 { continue; }
                in_body = true;
                skipped = 0;
            }
        } else {
            // Count depth changes in body lines.
            let opens: i32  = line.chars().filter(|&c| c == '{').count() as i32;
            let closes: i32 = line.chars().filter(|&c| c == '}').count() as i32;
            depth += opens - closes;
            skipped += 1;

            if depth <= 0 {
                // End of body — emit a collapsed placeholder.
                if skipped > 2 {
                    // Remove the last "{\n" we already emitted and replace with stub.
                    // Walk back in out to find the last '{' and truncate there.
                    if let Some(brace_pos) = out.rfind("{\n") {
                        out.truncate(brace_pos);
                        out.push_str(" { /* … */ }\n");
                    }
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
                in_body = false;
                depth   = 0;
            }
            // Otherwise silently skip body lines.
        }
    }
    out
}

// ── Python skeleton ───────────────────────────────────────────────────────────
fn skeleton_python(src: &str) -> String {
    let mut out        = String::new();
    let mut in_body    = false;
    let mut body_indent = 0usize;

    for line in src.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();

        if !in_body {
            // Keep class/def signatures, decorators, top-level assignments, imports.
            let keep = trimmed.starts_with("class ")
                || trimmed.starts_with("def ")
                || trimmed.starts_with("async def ")
                || trimmed.starts_with('@')
                || trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
                || trimmed.starts_with('#')
                || trimmed.is_empty()
                || indent == 0;

            if keep {
                out.push_str(line);
                out.push('\n');

                if (trimmed.starts_with("def ")
                    || trimmed.starts_with("async def ")) && trimmed.ends_with(':')
                {
                    in_body    = true;
                    body_indent = indent + 4;
                    out.push_str(&format!("{:indent$}...\n", "", indent = body_indent));
                }
            }
        } else {
            // Exit body when we return to the same or lower indent level.
            if !trimmed.is_empty() && indent <= body_indent.saturating_sub(4) {
                in_body = false;
                out.push_str(line);
                out.push('\n');
            }
            // else: silently skip body lines (we already wrote '...')
        }
    }
    out
}

// ── JS/TS skeleton ────────────────────────────────────────────────────────────
fn skeleton_js(src: &str) -> String {
    let mut out   = String::new();
    let mut depth = 0i32;
    let mut in_body = false;

    for line in src.lines() {
        let trimmed = line.trim();
        let opens:  i32 = line.chars().filter(|&c| c == '{').count() as i32;
        let closes: i32 = line.chars().filter(|&c| c == '}').count() as i32;

        let _is_sig = trimmed.starts_with("export ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("async function")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("//")
            || trimmed.starts_with("/**")
            || trimmed.starts_with(" * ")
            || trimmed.starts_with("*/")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("interface ")
            || trimmed.is_empty();

        if !in_body {
            out.push_str(line);
            out.push('\n');
            depth += opens - closes;

            if depth == 1 && opens > 0
                && (trimmed.contains("function ")
                    || trimmed.contains("=> {")
                    || trimmed.starts_with("class "))
            {
                in_body = true;
            }
        } else {
            depth += opens - closes;
            if depth <= 0 {
                if let Some(brace_pos) = out.rfind("{\n") {
                    out.truncate(brace_pos);
                    out.push_str(" { /* … */ }\n");
                }
                in_body = false;
                depth   = 0;
            }
        }
    }
    out
}

// ── Generic: first N lines ────────────────────────────────────────────────────
fn skeleton_head(src: &str, n: usize) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let total = lines.len();
    let taken = lines.into_iter().take(n).collect::<Vec<_>>().join("\n");
    if total > n {
        format!("{taken}\n// … ({} more lines — use [WESSAL:read] to fetch)\n", total - n)
    } else {
        taken
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stage 5 — BudgetPacker (the public API)
// ═══════════════════════════════════════════════════════════════════════════════

/// The assembled context ready to be sent to the LLM.
#[derive(Debug)]
pub struct ContextPacket {
    pub prompt:               String,
    pub token_estimate:       usize,
    pub files_full:           usize,
    pub files_skeleton:       usize,
    pub files_diff:           usize,
    pub files_summary:        usize,
    pub files_omitted:        usize,
    pub push_mode:            PushMode,
    pub budget:               TargetBudget,
}

/// Build the context packet for a single push.
///
/// # Arguments
/// * `current`    — live filesystem state
/// * `last_pushed` — what the LLM last received (empty on first push)
/// * `mode`       — what kind of push this is
/// * `url`        — the LLM page URL (used to derive the budget)
/// * `session_id` — stable ID for this Wessal session
pub fn build_packet(
    current:    &ProjectMap,
    last_pushed: &ProjectMap,
    mode:        PushMode,
    url:         &str,
    session_id:  &str,
) -> ContextPacket {
    let budget   = TargetBudget::from_url(url);
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Compute deltas between last push and current state.
    let deltas = compute_delta(last_pushed, current);

    // For Delta mode, only include files that changed.
    let files_to_consider: Vec<&FileEntry> = match &mode {
        PushMode::Delta => current
            .entries()
            .values()
            .filter(|e| deltas.iter().any(|d| d.path == e.path))
            .collect(),
        PushMode::OnDemand(path) => current
            .entries()
            .values()
            .filter(|e| &e.path == path)
            .collect(),
        PushMode::Initial => current.entries().values().collect(),
    };

    // Rank all candidate files.
    let ranked = rank_files_subset(&files_to_consider, &deltas, now_secs);

    // Budget allocation.
    let token_limit = match &mode {
        PushMode::Initial   => budget.initial,
        PushMode::Delta     => budget.delta,
        PushMode::OnDemand(_) => budget.on_demand,
    };

    // Reserve 15 % for the prompt header + summary footer.
    let file_budget = (token_limit as f64 * 0.85) as usize;

    let mut shaped:         Vec<ShapedFile> = Vec::new();
    let mut used_tokens                     = 0usize;
    let mut files_full                      = 0usize;
    let mut files_skeleton                  = 0usize;
    let mut files_diff                      = 0usize;
    let mut files_summary                   = 0usize;
    let mut files_omitted                   = 0usize;

    for (entry, is_delta) in &ranked {
        if used_tokens >= file_budget { files_omitted += 1; continue; }

        let delta_ref: Option<&FileDelta> = if *is_delta {
            deltas.iter().find(|d| &d.path == &entry.path)
        } else {
            None
        };

        let remaining = file_budget - used_tokens;
        let sf = shape_file(entry, delta_ref, remaining, budget.prefer_skeleton);

        match sf.kind {
            ShapeKind::Full     => files_full     += 1,
            ShapeKind::Skeleton => files_skeleton += 1,
            ShapeKind::Diff     => files_diff     += 1,
            ShapeKind::Summary  => files_summary  += 1,
        }

        used_tokens += sf.token_cost;
        shaped.push(sf);
    }

    // Render the prompt.
    let prompt = render_prompt(&shaped, &deltas, &mode, session_id, now_secs, &budget);

    info!(
        mode    = ?mode,
        tokens  = used_tokens,
        full    = files_full,
        skel    = files_skeleton,
        diff    = files_diff,
        summary = files_summary,
        omit    = files_omitted,
        "context packet built"
    );

    ContextPacket {
        prompt,
        token_estimate: used_tokens,
        files_full,
        files_skeleton,
        files_diff,
        files_summary,
        files_omitted,
        push_mode: mode,
        budget,
    }
}

// ── rank_files_subset: rank a pre-filtered slice ──────────────────────────────
fn rank_files_subset<'a>(
    files:   &[&'a FileEntry],
    deltas:  &[FileDelta],
    now:     u64,
) -> Vec<(&'a FileEntry, bool)> {
    let delta_paths: std::collections::HashSet<&PathBuf> =
        deltas.iter().map(|d| &d.path).collect();

    let mut ranked: Vec<(&FileEntry, bool, f64)> = files
        .iter()
        .map(|e| {
            let is_delta = delta_paths.contains(&e.path);
            let score    = score_file(e, is_delta, now);
            (*e, is_delta, score)
        })
        .collect();

    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().map(|(e, d, _)| (e, d)).collect()
}

// ── Prompt renderer ───────────────────────────────────────────────────────────
fn render_prompt(
    shaped:    &[ShapedFile],
    deltas:    &[FileDelta],
    mode:      &PushMode,
    session_id: &str,
    now_secs:  u64,
    budget:    &TargetBudget,
) -> String {
    let mode_tag = match mode {
        PushMode::Initial      => "INITIAL",
        PushMode::Delta        => "DELTA",
        PushMode::OnDemand(_)  => "ON-DEMAND",
    };

    let total_tokens: usize = shaped.iter().map(|s| s.token_cost).sum();

    let mut out = format!(
        "[WESSAL:{mode_tag} | session:{session_id} | t:{now_secs} | \
         files:{} | ~{total_tokens} tokens]\n\n",
        shaped.len(),
    );

    // ── Delta summary (shown first so the LLM sees what changed immediately) ─
    if !deltas.is_empty() && matches!(mode, PushMode::Delta | PushMode::Initial) {
        out.push_str("[CHANGES SINCE LAST PUSH]\n");
        for d in deltas {
            match &d.kind {
                DeltaKind::Added           =>
                    out.push_str(&format!("+ {}\n", d.path.display())),
                DeltaKind::Deleted         =>
                    out.push_str(&format!("- {}\n", d.path.display())),
                DeltaKind::Renamed { from } =>
                    out.push_str(&format!("→ {} → {}\n", from.display(), d.path.display())),
                DeltaKind::Modified        =>
                    out.push_str(&format!("~ {}\n", d.path.display())),
            }
        }
        out.push('\n');
    }

    // ── File sections ─────────────────────────────────────────────────────────
    for sf in shaped {
        let lang = sf.path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match &sf.kind {
            ShapeKind::Summary => {
                // Summary lines are already formatted — just append.
                out.push_str(&sf.content);
            }
            ShapeKind::Diff => {
                out.push_str(&format!(
                    "=== CHANGED: {} ===\n```diff\n{}\n```\n\n",
                    sf.path.display(),
                    sf.content.trim_end(),
                ));
            }
            ShapeKind::Skeleton => {
                out.push_str(&format!(
                    "=== SKELETON: {} ===\n```{lang}\n{}\n```\n\n",
                    sf.path.display(),
                    sf.content.trim_end(),
                ));
            }
            ShapeKind::Full => {
                out.push_str(&format!(
                    "=== FILE: {} ===\n```{lang}\n{}\n```\n\n",
                    sf.path.display(),
                    sf.content.trim_end(),
                ));
            }
        }
    }

    // ── Footer instructions ───────────────────────────────────────────────────
    if budget.prefer_skeleton {
        out.push_str(
            "[NOTE] Skeleton mode active (ChatGPT input limit). \
             To read a full file, respond with: [WESSAL:read:path/to/file.rs]\n"
        );
    }

    out.push_str("[END WESSAL CONTEXT]\n");
    out
}

// ── On-demand read request parser ─────────────────────────────────────────────

/// Scan an LLM response for `[WESSAL:read:path]` requests and return the paths.
pub fn parse_read_requests(llm_response: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pos   = 0;

    while pos < llm_response.len() {
        let Some(tag_start) = llm_response[pos..].find("[WESSAL:read:") else { break };
        let abs = pos + tag_start + "[WESSAL:read:".len();
        let rest = &llm_response[abs..];
        let Some(end) = rest.find(']') else { pos = abs; continue };
        let raw_path = rest[..end].trim();
        if !raw_path.is_empty() {
            paths.push(PathBuf::from(raw_path));
        }
        pos = abs + end + 1;
    }
    paths
}