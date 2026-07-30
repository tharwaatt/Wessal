# Wessal

Wessal is a local-first bridge daemon that connects web-based LLM interfaces (such as Claude, Gemini, and ChatGPT) to your local filesystem. It operates via a lightweight browser extension communicating with a local Rust daemon over WebSockets.

> The project is designed with a strict human-in-the-loop model: file read/write actions are explicitly initiated by the user.

---

## Architecture Overview

```
┌─────────────────────────┐          WebSocket           ┌─────────────────────────┐
│ Browser Extension       │ ───────────────────────────> │ Rust Daemon (wessal)    │
│ (DOM Collector)         │ <─────────────────────────── │ (Parser & Executor)     │
└─────────────────────────┘      paste / submit          └────────────┬────────────┘
                                                                      │
                                                                      ▼
                                                         ┌─────────────────────────┐
                                                         │ Local Filesystem & TUI  │
                                                         └─────────────────────────┘

```

1. **Browser Extension:** Extracts structured action tags from LLM responses on user demand.
2. **WebSocket Server:** Bridges the browser session with the local machine (`127.0.0.1:7878`).
3. **Daemon & Executor:** Parses actions, reads project state, executes atomic file writes, and performs three-way merges when conflicts occur.
4. **Terminal User Interface (TUI):** Displays real-time diffs, execution logs, and session statistics using `ratatui`.

---

## Action Tags Reference

Wessal recognizes structured tags emitted by the LLM in plain text or code blocks:

| Action Tag Syntax | Description |
| --- | --- |
| `[WESSAL:init]` | Scans project structure and returns initial metadata. |
| `[WESSAL:read:path/to/file]` | Reads the full content of a file. |
| `[WESSAL:read:path/to/file:start:count]` | Reads a specific line range from a file. |
| `[WESSAL:write:path/to/file]...[/WESSAL]` | Writes or updates a file (atomic complete overwrite). |
| `[WESSAL:ls:path/]` | Lists contents of a directory. |
| `[WESSAL:search:keyword]` | Searches the project for matching text. |
| `[WESSAL:outline:path/to/file]` | Extracts a structural skeleton of a source file. |

---

## Data Integrity and Safety

* **Atomic Writes:** File updates use temporary files and atomic renames to prevent partial writes or file corruption on failure.
* **Three-Way Merge:** Uses `diffy` to merge LLM changes with local modifications if a file was edited concurrently.
* **Path Validation:** Restricts operations to the target project directory, blocking path traversal (`../`) or absolute paths outside the project root.
* **Memory Bounded:** Implements explicit limits on file size reads and total content payloads to avoid high memory consumption.

---

## Installation

### 1. Building and Installing the Daemon

Requires Rust (cargo toolchain 1.75+).

```bash
git clone https://github.com/your-username/wessal.git
cd wessal
cargo install --path .

```

### 2. Installing the Browser Extension

1. Open Chrome (or any Chromium-based browser) and navigate to `chrome://extensions/`.
2. Enable **Developer mode** using the toggle switch in the top-right corner.
3. Click the **Load unpacked** button.
4. Select the `extension/` folder located within the cloned Wessal repository.

---

## Running

To start Wessal against a project directory:

```bash
cd /path/to/your/project
wessal .

```

---

## Active Development & Upcoming Features

The following architectural enhancements and subsystems are currently under design and active development:

### 1. Context Optimization & System Instructions

* **Project Guidance (`.wessal.md`):** Automated reading and injection of project-specific rules, style guides, and architecture instructions into the LLM system prompt upon `[WESSAL:init]`.
* **Refined Context Payloads:** Smarter initial payload construction prioritizing structural outlines and AST metadata over raw file dumps to reduce token overhead.

### 2. Global Memory & Data Persistence

* **Global Home Directory (`~/.wessal`):** Isolation of configuration, session logs, and databases from project workspaces into platform-native directories using the `dirs` crate.
* **Dual SQLite Architecture:**
* `state.db`: Durable, atomic database for tracking session history, transaction audits, and configuration state.
* `index.db`: Rebuildable derived cache for file hashes, symbol indexes, and structural outlines.



### 3. File Safety & Session Memory

* **Automated Pre-Write Backups:** Snapshots captured before executing any `[WESSAL:write]` action, stored as patch files linked to session metadata.
* **Rollback System (`[WESSAL:undo]`):** Single-command session or step rollbacks to safely revert unwanted LLM file modifications.

### 4. Real-Time Workspace Synchronization

* **Incremental File Watching:** Event-driven updates using the `notify` file-system watcher to maintain an up-to-date index without manual disk re-scans.

---

## License

This project is licensed under the [MIT License](https://www.google.com/search?q=LICENSE).