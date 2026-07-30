# Wessal

Wessal is a local-first bridge daemon that connects web-based LLM interfaces (such as Claude, Gemini, and ChatGPT) to your local filesystem. It operates via a lightweight browser extension communicating with a local Rust daemon over WebSockets.

The project is designed with a strict human-in-the-loop model: file read/write actions are explicitly initiated by the user.
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
| :--- | :--- |
| `[WESSAL:init]` | Scans project structure and returns initial metadata. |
| `[WESSAL:read:path/to/file]` | Reads the full content of a file. |
| `[WESSAL:read:path/to/file:start:count]` | Reads a specific line range from a file. |
| `[WESSAL:write:path/to/file]...[/WESSAL]` | Writes or updates a file (atomic complete overwrite). |
| `[WESSAL:ls:path/]` | Lists contents of a directory. |
| `[WESSAL:search:keyword]` | Searches the project for matching text. |
| `[WESSAL:outline:path/to/file]` | Extracts a structural skeleton of a source file. |

---

## Security and Data Integrity

- **Atomic Writes:** File updates use temporary files and atomic renames to prevent partial writes or file corruption on failure.
- **Three-Way Merge:** Uses `diffy` to merge LLM changes with local modifications if a file was edited concurrently.
- **Path Validation:** Restricts operations to the target project directory, blocking path traversal (`../`) or absolute paths outside the project root.
- **Memory Bounded:** Implements explicit limits on file size reads and total content payloads to avoid high memory consumption.

---

## Installation

### 1. Building and Installing the Daemon

Requires Rust (cargo toolchain 1.75+).

```bash
git clone [https://github.com/your-username/wessal.git](https://github.com/your-username/wessal.git)
cd wessal/wessal
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

## Roadmap (v8.0)

Planned architectural updates for upcoming releases:

* **Global State Isolation:** Migrating global configuration and sessions out of project roots into native user data directories (`dirs` crate).
* **SQLite Persistence (`state.db`):** Replacing JSON session logs with an atomic SQLite database for tracking history, audits, and rollback metadata.
* **Derived Indexing (`index.db`):** Rebuildable SQLite cache for project hashes, structural outlines, and fast AST search.
* **Rollback Mechanism (`[WESSAL:undo]`):** Automated local file backups prior to write actions, allowing single-command rollbacks.
* **Project Guidance (`.wessal.md`):** Automated injection of project-specific context and style rules during initialization.

---

## License

This project is licensed under the [MIT License](https://www.google.com/search?q=LICENSE).