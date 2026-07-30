// src/init_payload.rs
//
// Smart initialization payload builder for [WESSAL:init]
//
// Generates a token-efficient project overview with four sections:
//
//   1. Smart file tree — depth-limited, budget-capped, box-drawing chars,
//      file sizes, and collapsed subtrees with hidden-file counts
//   2. README.md excerpt — first N lines to give the LLM project context
//   3. .wessal/instructions.md — persistent developer session instructions
//   4. .wessal/memory.md — persistent project memory / state
//
// Design goals
// ─────────────
// • Never overflow the LLM's context window, no matter how large the project
// • Show the most useful structure at a glance; let the LLM drill down with ls:
// • Zero allocations in the hot path (file entries come from ProjectMap in RAM)
// • All filesystem I/O is synchronous and cheap (only 2–3 small files)

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

use tracing::debug;

use crate::context::ProjectMap;

// ══════════════════════════════════════════════════════════════════════════════
// Public configuration
// ══════════════════════════════════════════════════════════════════════════════

/// Controls how the smart file tree is rendered.
///
/// All fields have sensible defaults via [`SmartTreeConfig::default`].
#[derive(Debug, Clone)]
pub struct SmartTreeConfig {
    /// Maximum directory nesting depth to expand before collapsing.
    ///
    /// With `max_depth = 3` the tree shows three levels of directories.
    /// Any directory that would appear at level 4+ is folded to
    /// `dir/  (+ N hidden files)`.  Default: `3`.
    pub max_depth: usize,

    /// Hard cap on the number of rendered tree lines.
    ///
    /// Once this is reached, remaining items in the current directory are
    /// collapsed in a single summary line.  Default: `150`.
    pub max_lines: usize,

    /// How many lines of README.md to include in the payload.  Default: `50`.
    pub readme_lines: usize,
}

impl Default for SmartTreeConfig {
    fn default() -> Self {
        Self {
            max_depth:    3,
            max_lines:    150,
            readme_lines: 50,
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Public entry point
// ══════════════════════════════════════════════════════════════════════════════

/// Build the complete `[WESSAL:init]` response payload.
///
/// Combines a smart file tree, README excerpt, and `.wessal/` context files
/// into a single token-efficient string for pasting into an LLM chat window.
///
/// # Arguments
///
/// * `map`          — Current [`ProjectMap`] (already scanned).
/// * `project_root` — Absolute path to the project root on disk.
/// * `config`       — Tree rendering options.
pub fn build_init_payload(
    map:          &ProjectMap,
    project_root: &Path,
    config:       &SmartTreeConfig,
) -> String {
    let entries     = map.entries();
    let total_files = entries.len();
    let total_bytes: u64 = entries.values().map(|e| e.size_bytes).sum();

    debug!(
        total_files,
        total_bytes,
        max_depth    = config.max_depth,
        max_lines    = config.max_lines,
        readme_lines = config.readme_lines,
        "building init payload"
    );

    // ── 1. Smart file tree ────────────────────────────────────────────────────
    let tree = build_smart_tree(
        entries.iter().map(|(p, e)| (p.as_path(), e.size_bytes)),
        config,
    );

    // ── 2. README excerpt ─────────────────────────────────────────────────────
    let readme = read_readme_excerpt(project_root, config.readme_lines);

    // ── 3. .wessal/ context files ─────────────────────────────────────────────
    let instructions = read_wessal_file(project_root, "instructions.md");
    let memory       = read_wessal_file(project_root, "memory.md");

    // ── 4. Assemble the final payload ─────────────────────────────────────────
    let mut out = String::with_capacity(4_096);

    out.push_str("[WESSAL RESULT]\n");
    out.push_str("=== PROJECT INITIALIZED ===\n");
    out.push_str(&format!(
        "Total Tracked: {} files ({})\n\n",
        total_files,
        format_size(total_bytes),
    ));

    out.push_str("=== SMART FILE TREE ===\n");
    out.push_str(&tree);
    out.push_str(
        "[Note: Deep or large directories are collapsed to save context. \
         Use `[WESSAL:ls:path/]` to explore them.]\n\n",
    );

    if let Some(readme_text) = readme {
        out.push_str("=== PROJECT README (Excerpt) ===\n");
        out.push_str(&readme_text);
        out.push('\n');
    }

    if let Some(instr) = instructions {
        out.push_str("=== DEVELOPER INSTRUCTIONS ===\n");
        out.push_str(&instr);
        out.push('\n');
    }

    if let Some(mem) = memory {
        out.push_str("=== PROJECT MEMORY ===\n");
        out.push_str(&mem);
        out.push('\n');
    }

    out.push_str("[System ready]\n\nUser message: ");
    out
}

// ══════════════════════════════════════════════════════════════════════════════
// In-memory directory tree
// ══════════════════════════════════════════════════════════════════════════════

/// An in-memory directory node built from the flat `ProjectMap`.
///
/// Both `files` and `subdirs` use [`BTreeMap`] so iteration is always
/// alphabetically sorted — the tree output is therefore deterministic.
#[derive(Default)]
struct DirNode {
    /// Direct files in this directory: filename → size_bytes.
    files: BTreeMap<String, u64>,
    /// Immediate subdirectories.
    subdirs: BTreeMap<String, DirNode>,
}

impl DirNode {
    /// Total number of files anywhere in this subtree (recursive).
    fn total_file_count(&self) -> usize {
        self.files.len()
            + self.subdirs.values().map(|d| d.total_file_count()).sum::<usize>()
    }
}

/// Recursively insert a relative file path into the tree.
fn insert_path(node: &mut DirNode, parts: &[&str], size: u64) {
    match parts {
        // Leaf — record the file
        [name] => {
            node.files.insert((*name).to_string(), size);
        }
        // Non-leaf — create (or enter) the subdirectory and recurse
        [dir, rest @ ..] => {
            insert_path(
                node.subdirs.entry((*dir).to_string()).or_default(),
                rest,
                size,
            );
        }
        // Empty slice (should never happen for valid paths)
        [] => {}
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Tree renderer
// ══════════════════════════════════════════════════════════════════════════════

struct Renderer {
    lines:     Vec<String>,
    max_lines: usize,
    max_depth: usize,
}

impl Renderer {
    fn new(cfg: &SmartTreeConfig) -> Self {
        Self {
            lines:     Vec::with_capacity(cfg.max_lines.min(256)),
            max_lines: cfg.max_lines,
            max_depth: cfg.max_depth,
        }
    }

    #[inline]
    fn over_budget(&self) -> bool {
        self.lines.len() >= self.max_lines
    }

    /// Render the children of `node` at the given nesting `depth`.
    ///
    /// - `prefix` — continuation prefix string (box-drawing indentation)
    /// - `depth`  — 0 = root's direct children; increments when descending
    fn render_children(&mut self, node: &DirNode, prefix: &str, depth: usize) {
        let n_dirs  = node.subdirs.len();
        let n_files = node.files.len();
        let total   = n_dirs + n_files;
        if total == 0 {
            return;
        }

        // `idx` tracks our position across ALL children (dirs first, then files).
        // It is used to determine is_last and to compute hidden-file counts.
        let mut idx = 0usize;

        // ── Subdirectories (alphabetically sorted by BTreeMap) ────────────────
        for (name, subdir) in &node.subdirs {
            if self.over_budget() {
                // Sum file descendants for all subdirs from `idx` onward, plus
                // every direct file in the current node (none rendered yet).
                let hidden: usize = node.subdirs.values()
                    .skip(idx)
                    .map(|d| d.total_file_count())
                    .sum::<usize>()
                    + n_files;
                self.push_overflow(prefix, hidden);
                return;
            }

            let is_last   = idx == total - 1;
            let conn      = connector(is_last);
            let child_pfx = format!("{}{}", prefix, extension(is_last));
            let file_cnt  = subdir.total_file_count();

            // Collapse when the next level would exceed max_depth.
            // `depth` is the depth of *current* node; the subdir is at depth+1.
            if depth + 1 >= self.max_depth {
                self.lines.push(format!(
                    "{prefix}{conn}{name}/  (+ {file_cnt} hidden files)"
                ));
            } else {
                self.lines.push(format!("{prefix}{conn}{name}/"));
                if file_cnt > 0 {
                    self.render_children(subdir, &child_pfx, depth + 1);
                }
            }

            idx += 1;
        }

        // ── Files (alphabetically sorted by BTreeMap) ─────────────────────────
        for (name, &size) in &node.files {
            if self.over_budget() {
                // `idx - n_dirs` = number of files already rendered in this dir
                let hidden = n_files - (idx - n_dirs);
                self.push_overflow(prefix, hidden);
                return;
            }

            let is_last = idx == total - 1;
            let conn    = connector(is_last);
            self.lines.push(format!(
                "{prefix}{conn}{name} ({})",
                format_size(size)
            ));

            idx += 1;
        }
    }

    fn push_overflow(&mut self, prefix: &str, hidden: usize) {
        self.lines.push(format!(
            "{prefix}└── ... (+ {hidden} more items — use `[WESSAL:ls:path/]`)"
        ));
    }

    fn finish(self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        let mut out = self.lines.join("\n");
        out.push('\n');
        out
    }
}

/// Box-drawing connector for the current child entry.
#[inline]
fn connector(is_last: bool) -> &'static str {
    if is_last { "└── " } else { "├── " }
}

/// Box-drawing continuation prefix for children of the current entry.
#[inline]
fn extension(is_last: bool) -> &'static str {
    if is_last { "    " } else { "│   " }
}

// ══════════════════════════════════════════════════════════════════════════════
// Public tree builder
// ══════════════════════════════════════════════════════════════════════════════

/// Build a depth-limited, budget-capped ASCII file tree.
///
/// Directories deeper than `config.max_depth` are collapsed to
/// `dir/  (+ N hidden files)`.  Once `config.max_lines` lines are emitted,
/// the remainder of the current directory is folded into a single
/// "more items" summary line.
///
/// Returns the rendered tree as a `String` (trailing newline included).
fn build_smart_tree<'a>(
    paths:  impl Iterator<Item = (&'a Path, u64)>,
    config: &SmartTreeConfig,
) -> String {
    // Build the in-memory tree from the flat path list
    let mut root = DirNode::default();
    for (path, size) in paths {
        let parts: Vec<&str> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _                   => None,
            })
            .collect();
        if !parts.is_empty() {
            insert_path(&mut root, &parts, size);
        }
    }

    let mut renderer = Renderer::new(config);
    renderer.render_children(&root, "", 0);
    renderer.finish()
}

// ══════════════════════════════════════════════════════════════════════════════
// .wessal/ file reader
// ══════════════════════════════════════════════════════════════════════════════

/// Read a file from the `.wessal/` hidden directory at `project_root`.
///
/// Returns `None` if the file does not exist or cannot be read as UTF-8.
/// Trailing whitespace is trimmed; a single trailing newline is appended.
fn read_wessal_file(project_root: &Path, filename: &str) -> Option<String> {
    let path = project_root.join(".wessal").join(filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            debug!(
                "Loaded .wessal/{filename} ({} bytes)",
                content.len()
            );
            let mut out = content.trim_end().to_string();
            out.push('\n');
            Some(out)
        }
        Err(_) => {
            debug!(".wessal/{filename} — not found, skipping");
            None
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// README excerpt reader
// ══════════════════════════════════════════════════════════════════════════════

/// Find the first `README.md` (case-insensitive) directly inside `dir`.
///
/// Returns `None` if no match is found.
fn find_readme(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            name == "readme.md" && e.path().is_file()
        })
        .map(|e| e.path())
}

/// Read up to `max_lines` lines from the project README (case-insensitive match).
///
/// If the file is longer, a truncation notice is appended pointing to the
/// `[WESSAL:read:]` tag for the full file.
///
/// Returns `None` if no README is present.
fn read_readme_excerpt(project_root: &Path, max_lines: usize) -> Option<String> {
    let readme_path = find_readme(project_root)?;

    let content = match std::fs::read_to_string(&readme_path) {
        Ok(c)  => c,
        Err(e) => {
            debug!("Failed to read {}: {e}", readme_path.display());
            return None;
        }
    };

    let total_lines = content.lines().count();
    let truncated   = total_lines > max_lines;

    let excerpt: String = content
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");

    let readme_rel = readme_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "README.md".to_string());

    let mut out = excerpt;
    if truncated {
        out.push_str(&format!(
            "\n... (showing {max_lines}/{total_lines} lines — \
             use `[WESSAL:read:{readme_rel}]` for the full file)"
        ));
    }
    out.push('\n');

    debug!(
        "README excerpt: {}/{total_lines} lines from {}",
        max_lines.min(total_lines),
        readme_path.display(),
    );

    Some(out)
}

// ══════════════════════════════════════════════════════════════════════════════
// Formatting helpers
// ══════════════════════════════════════════════════════════════════════════════

/// Format a byte count as a human-readable string.
///
/// ```text
/// 0         → "0 B"
/// 512       → "512 B"
/// 1_024     → "1.0 KB"
/// 1_572_864 → "1.5 MB"
/// ```
fn format_size(bytes: u64) -> String {
    match bytes {
        0                    => "0 B".to_string(),
        b if b < 1_024       => format!("{b} B"),
        b if b < 1_024 * 1_024 => format!("{:.1} KB", b as f64 / 1_024.0),
        b                    => format!("{:.1} MB", b as f64 / (1_024.0 * 1_024.0)),
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Unit tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_size ───────────────────────────────────────────────────────────

    #[test]
    fn test_format_size_zero() {
        assert_eq!(format_size(0), "0 B");
    }

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1_023), "1023 B");
    }

    #[test]
    fn test_format_size_kb() {
        assert_eq!(format_size(1_024),       "1.0 KB");
        assert_eq!(format_size(2_048),       "2.0 KB");
        assert_eq!(format_size(1_536),       "1.5 KB");
        assert_eq!(format_size(1_024 * 1_024 - 1), "1024.0 KB");
    }

    #[test]
    fn test_format_size_mb() {
        assert_eq!(format_size(1_048_576), "1.0 MB");
        assert_eq!(format_size(2_097_152), "2.0 MB");
    }

    // ── Helper: build tree from string specs ──────────────────────────────────

    fn tree_from(specs: &[(&str, u64)], cfg: &SmartTreeConfig) -> String {
        let owned: Vec<(PathBuf, u64)> =
            specs.iter().map(|(p, s)| (PathBuf::from(p), *s)).collect();
        build_smart_tree(
            owned.iter().map(|(p, s)| (p.as_path(), *s)),
            cfg,
        )
    }

    // ── build_smart_tree ──────────────────────────────────────────────────────

    #[test]
    fn test_empty_project() {
        let cfg  = SmartTreeConfig::default();
        let tree = build_smart_tree(std::iter::empty(), &cfg);
        assert!(tree.is_empty(), "empty project should produce empty tree");
    }

    #[test]
    fn test_single_root_file() {
        let cfg  = SmartTreeConfig::default();
        let tree = tree_from(&[("Cargo.toml", 1_024)], &cfg);
        assert!(tree.contains("Cargo.toml"), "file name must appear");
        assert!(tree.contains("1.0 KB"),     "size must appear");
    }

    #[test]
    fn test_last_item_uses_corner_connector() {
        let cfg  = SmartTreeConfig::default();
        let tree = tree_from(&[("only.rs", 500)], &cfg);
        // A single child is always "last", so it uses └──
        assert!(tree.contains("└── only.rs"), "single item should use └──");
    }

    #[test]
    fn test_non_last_item_uses_branch_connector() {
        let cfg  = SmartTreeConfig::default();
        let tree = tree_from(&[("a.rs", 100), ("b.rs", 200)], &cfg);
        // "a.rs" is not last → ├──; "b.rs" is last → └──
        assert!(tree.contains("├── a.rs"), "non-last item should use ├──");
        assert!(tree.contains("└── b.rs"), "last item should use └──");
    }

    #[test]
    fn test_nested_structure_expands_within_max_depth() {
        let cfg = SmartTreeConfig::default(); // max_depth = 3
        let tree = tree_from(&[
            ("src/main.rs",       5_120),
            ("src/utils/math.rs", 1_024),
            ("Cargo.toml",        1_024),
        ], &cfg);

        assert!(tree.contains("src/"),    "src/ dir must appear");
        assert!(tree.contains("utils/"),  "utils/ must expand (within depth)");
        assert!(tree.contains("math.rs"), "math.rs must be visible");
        assert!(tree.contains("Cargo.toml"), "root file must appear");
    }

    #[test]
    fn test_depth_collapse_at_max_depth() {
        // max_depth = 2: root(0) → src(1) → collapse at depth+1=2 ≥ 2
        let cfg = SmartTreeConfig { max_depth: 2, max_lines: 150, readme_lines: 50 };
        let tree = tree_from(&[
            ("src/main.rs",              5_120),
            ("src/utils/math.rs",        1_024),
            ("src/utils/nested/deep.rs",   512),
            ("Cargo.toml",               1_024),
        ], &cfg);

        // src/ shows up, but utils/ should be collapsed since it is at depth 1
        // and depth+1 = 2 >= max_depth = 2
        assert!(tree.contains("src/"),                    "src/ must appear");
        assert!(tree.contains("utils/"),                  "utils/ must appear (collapsed)");
        assert!(tree.contains("hidden files"),            "utils/ must be collapsed");
        assert!(!tree.contains("math.rs"),                "math.rs must be hidden");
    }

    #[test]
    fn test_collapse_shows_correct_hidden_count() {
        // max_depth=1: all subdirs collapsed immediately
        let cfg = SmartTreeConfig { max_depth: 1, max_lines: 150, readme_lines: 50 };
        let tree = tree_from(&[
            ("src/a.rs", 100),
            ("src/b.rs", 200),
            ("src/c.rs", 300),
        ], &cfg);

        // src/ should report 3 hidden files
        assert!(tree.contains("3 hidden files"), "hidden count should be 3");
    }

    #[test]
    fn test_line_budget_truncation() {
        let cfg = SmartTreeConfig { max_depth: 10, max_lines: 3, readme_lines: 50 };
        let tree = tree_from(&[
            ("a.rs", 100), ("b.rs", 200), ("c.rs", 300),
            ("d.rs", 400), ("e.rs", 500),
        ], &cfg);

        let lines: Vec<&str> = tree.trim_end().lines().collect();
        // With max_lines=3 we get: a.rs, b.rs, then the overflow line
        assert!(
            lines.len() <= 4,  // at most one extra for the overflow line
            "tree should be budget-capped; got {} lines:\n{tree}",
            lines.len()
        );
        assert!(
            tree.contains("more items"),
            "overflow message should appear"
        );
    }

    #[test]
    fn test_dirs_before_files() {
        let cfg = SmartTreeConfig::default();
        let tree = tree_from(&[
            ("z_file.rs",    100),
            ("a_dir/x.rs",   200),
        ], &cfg);

        // a_dir/ should appear before z_file.rs regardless of sort order
        let dir_pos  = tree.find("a_dir/").unwrap_or(usize::MAX);
        let file_pos = tree.find("z_file.rs").unwrap_or(usize::MAX);
        assert!(
            dir_pos < file_pos,
            "directories must appear before files:\n{tree}"
        );
    }

    #[test]
    fn test_continuation_prefix_in_subdirectory() {
        let cfg = SmartTreeConfig::default();
        let tree = tree_from(&[
            ("src/a.rs",      100),
            ("src/b.rs",      200),
            ("Cargo.toml",  1_024),
        ], &cfg);

        // src/ is not last (Cargo.toml comes after in file-after-dir ordering)
        // so its contents should have "│   " continuation prefix
        assert!(
            tree.contains("│   "),
            "non-last dir should use │   continuation:\n{tree}"
        );
    }

    // ── insert_path edge cases ────────────────────────────────────────────────

    #[test]
    fn test_insert_empty_parts_is_noop() {
        let mut root = DirNode::default();
        insert_path(&mut root, &[], 0);
        assert!(root.files.is_empty());
        assert!(root.subdirs.is_empty());
    }

    #[test]
    fn test_total_file_count() {
        let mut root = DirNode::default();
        insert_path(&mut root, &["a.rs"], 100);
        insert_path(&mut root, &["src", "b.rs"], 200);
        insert_path(&mut root, &["src", "utils", "c.rs"], 300);
        assert_eq!(root.total_file_count(), 3);
        assert_eq!(root.subdirs["src"].total_file_count(), 2);
    }
}