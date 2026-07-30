// src/actions.rs — Minimalist LLM Action Parser
//
// Supported tags:
//   [WESSAL:init]
//   [WESSAL:read:path]
//   [WESSAL:read:path:45:80]
//   [WESSAL:outline:path]
//   [WESSAL:search:keyword]
//   [WESSAL:write:path]...code...[/WESSAL]
//   [WESSAL:ls:path]

use std::path::PathBuf;
use tracing::debug;

// ══════════════════════════════════════════════════════════════════════════════
// ReadOptions
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ReadOptions {
    pub offset: usize,
    pub limit:  usize,
}

impl Default for ReadOptions {
    fn default() -> Self { Self { offset: 1, limit: 300 } }
}

// ══════════════════════════════════════════════════════════════════════════════
// Action
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub enum Action {
    Init,
    ReadFile {
        path:    PathBuf,
        options: ReadOptions,
    },
    Outline(PathBuf),
    Search(String),
    WriteFile {
        path:    PathBuf,
        content: String,
        lang:    String,
    },
    ListDir(PathBuf),
}

impl Action {
    pub fn preview(&self) -> String {
        match self {
            Action::Init => "init: project structure".to_string(),
            Action::Outline(p) => format!("outline: {}", p.display()),
            Action::Search(k) => format!("search: {}", k),
            Action::ReadFile { path, options } => {
                if options.offset == 1 && options.limit == 300 {
                    format!("read: {}", path.display())
                } else {
                    format!("read: {} L{}–L{}", path.display(), options.offset, options.offset + options.limit - 1)
                }
            }
            Action::WriteFile { path, .. } => format!("write: {}", path.display()),
            Action::ListDir(p)             => format!("ls: {}", p.display()),
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// parse_all_actions
// ══════════════════════════════════════════════════════════════════════════════

pub fn parse_all_actions(text: &str) -> Vec<Action> {
    let cleaned = strip_backtick_wrappers(text);
    let mut actions = Vec::new();
    let mut pos = 0;

    while pos < cleaned.len() {
        let Some(tag_pos) = cleaned[pos..].find('[') else { break };
        let abs   = pos + tag_pos;
        let after = &cleaned[abs + 1..];
        let Some(close) = after.find(']') else { pos = abs + 1; continue };
        let inner = after[..close].trim();

        if inner.eq_ignore_ascii_case("WESSAL:init") || inner.eq_ignore_ascii_case("WESSAL: init") {
            actions.push(Action::Init);
            pos = abs + 1 + close + 1;

        } else if let Some(rest) = ci_strip(inner, "WESSAL:outline:").or_else(|| ci_strip(inner, "WESSAL: outline :")) {
            let p = rest.trim();
            if !p.is_empty() { actions.push(Action::Outline(PathBuf::from(p))); }
            pos = abs + 1 + close + 1;

        } else if let Some(rest) = ci_strip(inner, "WESSAL:search:").or_else(|| ci_strip(inner, "WESSAL: search :")) {
            let k = rest.trim();
            if !k.is_empty() { actions.push(Action::Search(k.to_string())); }
            pos = abs + 1 + close + 1;

        } else if let Some(rest) = ci_strip(inner, "WESSAL:read:").or_else(|| ci_strip(inner, "WESSAL: read :")) {
            if let Some(action) = parse_read(rest.trim()) { actions.push(action); }
            pos = abs + 1 + close + 1;

        } else if let Some(rest) = ci_strip(inner, "WESSAL:ls:").or_else(|| ci_strip(inner, "WESSAL: ls :")) {
            let p = rest.trim();
            actions.push(Action::ListDir(PathBuf::from(if p.is_empty() { "." } else { p })));
            pos = abs + 1 + close + 1;

        } else if let Some(raw_path) = ci_strip(inner, "WESSAL:write:")
                                  .or_else(|| ci_strip(inner, "DEVMATE:write:"))
                                  .or_else(|| ci_strip(inner, "WESSAL: write :"))
                                  .or_else(|| ci_strip(inner, "DEVMATE: write :")) {
            let path       = raw_path.trim().to_string();
            let body_start = abs + 1 + close + 1;
            if let Some((action, end)) = parse_write_body(&cleaned, body_start, path) {
                actions.push(action);
                pos = end;
            } else { pos = body_start; }
        } else {
            pos = abs + 1;
        }
    }

    let mut seen = std::collections::HashSet::new();
    let actions: Vec<Action> = actions.into_iter().filter(|a| {
        let key = match a {
            Action::Init => "init".to_string(),
            Action::Outline(p) => format!("outline:{}", p.display()),
            Action::Search(k) => format!("search:{}", k),
            Action::ReadFile { path, options } => format!("read:{}:{}:{}", path.display(), options.offset, options.limit),
            Action::WriteFile { path, .. } => format!("write:{}", path.display()),
            Action::ListDir(p) => format!("ls:{}", p.display()),
        };
        seen.insert(key)
    }).collect();

    debug!("parse_all_actions: {} action(s) found (after dedup)", actions.len());
    actions
}

fn parse_read(spec: &str) -> Option<Action> {
    if spec.is_empty() { return None; }
    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    if parts[0].trim().is_empty() { return None; }

    let path = PathBuf::from(parts[0].trim());
    let mut opts = ReadOptions::default();

    if parts.len() >= 3 {
        opts.offset = parse_num_param(parts[1], "offset").unwrap_or(1).max(1);
        opts.limit  = parse_num_param(parts[2], "limit").unwrap_or(300).max(1);
    } else if parts.len() == 2 {
        opts.offset = parse_num_param(parts[1], "offset").unwrap_or(1).max(1);
    }

    Some(Action::ReadFile { path, options: opts })
}

fn parse_num_param(s: &str, key: &str) -> Option<usize> {
    let s   = s.trim();
    let val = if let Some(eq) = s.find('=') {
        if s[..eq].trim().eq_ignore_ascii_case(key) { &s[eq+1..] } else { s }
    } else { s };
    val.trim().parse().ok()
}

fn parse_write_body(text: &str, start: usize, raw_path: String) -> Option<(Action, usize)> {
    let rest         = &text[start..];
    let rest_trimmed = rest.trim_start_matches('\n');
    let trim_offset  = rest.len() - rest_trimmed.len();
    let abs_rest     = start + trim_offset;
    let rest         = rest_trimmed;

    let close_pos   = rest.find("[/WESSAL]").or_else(|| rest.find("[/DEVMATE]"))?;
    let body        = rest[..close_pos].trim();
    let closer_len  = if rest[close_pos..].starts_with("[/WESSAL]") { 9 } else { 10 };
    let end         = abs_rest + close_pos + closer_len;

    let (lang, content) = if body.starts_with("```") {
        let after = body[3..].trim_start_matches('\n');
        let nl    = after.find('\n').unwrap_or(after.len());
        let lang  = after[..nl].trim().to_string();
        let code  = after[nl..].trim_start_matches('\n');
        let code  = code.strip_suffix("```").unwrap_or(code).trim_end().to_string();
        (lang, code)
    } else {
        (String::new(), body.to_string())
    };

    Some((Action::WriteFile { path: PathBuf::from(raw_path), content: content.trim_end().to_string(), lang }, end))
}

fn strip_backtick_wrappers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i   = 0;
    while i < chars.len() {
        if chars[i] == '`' && i + 1 < chars.len() && chars[i+1] == '[' {
            let rest: String = chars[i+1..].iter().collect();
            if rest.starts_with("[WESSAL:") || rest.starts_with("[DEVMATE:") {
                i += 1; continue;
            }
        }
        if chars[i] == ']' && i + 1 < chars.len() && chars[i+1] == '`' {
            out.push(']');
            i += 2; continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn ci_strip<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) { Some(&s[prefix.len()..]) } else { None }
}