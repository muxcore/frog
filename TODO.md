# Frog - Terminal Oracle Client

A terminal-based Oracle database client written in Rust, inspired by Toad, tmux, btop, and Hyprland.

## Build

```bash
cargo build --release
# Binary: ./target/release/frog
```

## Run

```bash
# CLI args (psql-style)
./target/release/frog -H localhost -P 1521 -S ORCL -U user

# Env vars
export ORACLE_HOST=localhost ORACLE_PORT=1521 ORACLE_SERVICE=ORCL ORACLE_USER=u ORACLE_PASSWORD=p
./target/release/frog
```

Requires Oracle Instant Client (`libclntsh.so`) — install via AUR `oracle-instantclient-basic` and set `LD_LIBRARY_PATH`.

## Keyboard Reference (current)

| Key | Action |
|---|---|
| `Ctrl+Enter` | Execute statement at cursor |
| `F5` | Run all statements as script |
| `Ctrl+:` | Command bar (e.g. `@file.sql`, `@@inc.sql`, `clear`, `cls`) |
| `Tab` | Cycle focus: Editor / Results / Sidebar |
| `Ctrl+M` | Maximize/restore focused panel |
| `Ctrl+B` | Toggle sidebar |
| `Ctrl+Y` | Copy results to clipboard |
| `Ctrl+D` | Toggle Markdown table format |
| `Ctrl+O` | Connection dialog |
| `Ctrl+C` | Cancel query |
| `Ctrl+T/W` | New / close session tab |
| `Ctrl+Left/Right` | Switch session tabs |
| `F1/F2/F3` | Help / v$session / History |
| `Ctrl+Q` | Quit |
| Mouse | Hover tabs to focus, drag splitter to resize, click panel to focus, scroll in results |

## Project Structure

```
src/
  main.rs              # Entry point
  cli/mod.rs           # CLI args + env vars + YAML config
  db/
    mod.rs             # Re-exports
    connection.rs      # OracleConnection wrapper (oracle crate / ODPI-C)
    session_manager.rs # Sessions, SqlEditor, SessionManager, history persistence
  tui/
    mod.rs             # Re-exports App
    app.rs             # Event loop, key/mouse handling, layout, focus management
    widgets.rs         # Tabs, status bar, table viewer, help, dialogs, panels
```

## Done

- [x] CLI + env var connection params (`ORACLE_HOST/PORT/SERVICE/USER/PASSWORD/CONNECT`)
- [x] YAML config file support (`~/.config/frog/config.yml`)
- [x] Multi-session tabs (tmux-style) with hover-focus
- [x] Interactive connection dialog (`Ctrl+O`) with error feedback
- [x] Background thread query execution + cancel plumbing
- [x] v$session overview (F2)
- [x] Pretty-printed result table, first 100 rows, 2D scrolling
- [x] Hyprland-style mouse: hover focus, drag-to-resize splitter, click-to-focus panels
- [x] SqlEditor with real cursor row/col, arrow navigation, Home/End
- [x] Statement splitting on `;` and `/` lines; current-statement detection at cursor
- [x] `Ctrl+Enter` = run statement under cursor; `F5` = run all as script
- [x] Real blinking terminal cursor positioned in editor
- [x] Statement-at-cursor highlighting (in editor renderer)
- [x] Clipboard copy of results (`Ctrl+Y`), Markdown format toggle (`Ctrl+D`)
- [x] **SQL history persisted to `frog_history.txt` next to binary** (load on start, append with `-- frog@timestamp` markers per executed statement)
- [x] **VERIFY BUILD** — Synced `widgets.rs` signatures with `app.rs` (`render_editor`, `render_history`, `render_status_bar`, `render_table`) and verified clean `cargo check`.
- [x] Editor: vertical scroll follows cursor (`ensure_cursor_visible`) wired in render loop.
- [x] Query execution feedback signal (`⟳ RUNNING` tab badge, glowing yellow editor border, status bar badge, and result viewer placeholder).
- [x] Dynamic vertical editor resizing via `Ctrl+Plus`/`Ctrl+=`/`Ctrl+Up`/`Alt+Up` and `Ctrl+-`/`Ctrl+_`/`Ctrl+Down`/`Alt+Down`.
- [x] **tmux-style command bar (`Ctrl+:`)** — replaces the status bar with a `:`-prompt input. `@file.sql` runs scripts (incl. nested `@@inc`), `clear`/`cls` clears results; unknown commands show inline errors.
- [x] **`@file` scripting moved out of the SQL editor** into the command bar (was previously triggered by typing `@file.sql` in the editor).
- [x] **Result-viewer scrolling fixed for Markdown & ASCII formats** — vertical (and horizontal) scroll now applies in all three result formats, not just Table. (Also rebuilt the stale release binary.)
- [x] **Low-priority audit cleanups** — status-message auto-expiry (5s TTL), pagination-error status-line UX, `@file` non-`.sql` confirmation prompt, `null_display` config wired, removed unused `connect_string` param, zero clippy warnings, `render_table` arg-grouping (`TableViewState`), and friendly `DPI-1047` connect hint.

## TODO / Known Issues

> Audited as of Aug 2026. Items grouped by focus; priority: High / Medium / Low.

### Correctness & SQL Parsing

- [x] **[High] Statement splitter is not SQL-aware.** Rewrote `SqlEditor::statements_with_ranges` to track quoting and `--`/`/* */` comments so `;` inside them is not a terminator, and to keep PL/SQL blocks (`DECLARE`/`BEGIN`/`CREATE ... PROCEDURE|FUNCTION|PACKAGE|...`) as single statements that terminate at `END;`. Trailing `/` no longer leaks into a statement. Covered by unit tests.
- [x] **[Medium] `@file` expansion has no dotted-identifier / missing-file hint.** `load_script_file` now returns a clear message when the file is not found, showing the attempted path and explaining `@` (CWD) vs `@@` (relative-to-including-file) resolution rules.

### Concurrency & Resource Lifecycle

- [x] **[High] Oracle connection leaks on session close.** `remove_session` now removes the connection from `conn_map` and calls the new `OracleConnection::close()` so the DB session is released promptly.
- [x] **[High] Unbounded in-memory growth.** Added `max_history` (from config) and `max_results_per_session` bounds; `query_history` drops oldest entries over the cap and `session.results` trims oldest pages over the cap.
- [x] **[Medium] One OS thread per execution.** Added a per-session `job_seq` generation token carried on every result; `poll_result` now drops results that were produced by a superseded execution, so rapid `Ctrl+Enter` no longer interleaves or displays stale rows from an older spawn.
- [x] **[Medium] Cancel/execute race on shared connection.** Serialized execution with a per-session `exec_lock` (`Arc<Mutex<()>>`) so two worker threads never call ODPI-C concurrently on the same connection; `break_execution` remains callable from the UI thread while a query runs.

### Configuration Wiring / Dead Code

- [x] **[High] `autocommit` config is unused.** Added `connect_with_autocommit` and wired `Config.autocommit` through `SessionManager.autocommit` into every connection (initial, connection-dialog, and new-session auto-connect). Connections now honor the configured autocommit.
- [x] **[Medium] `max_rows`/`FROG_MAX_ROWS` is ignored.** Added `SessionManager.max_rows` (wired from config) and apply it as a soft cap in `fetch_next_page` — pagination stops and informs the user once the total rows fetched reaches the limit.
- [x] **[Low] `UiConfig` (theme, tab_size, date_format, null_display) is dead config.** `null_display` is now applied to every NULL cell in results (wired through `OracleConnection.null_display`). `date_format`/`theme` are not applicable: Oracle returns dates as pre-formatted strings and the TUI uses a fixed palette.

### Security & Secrets

- [x] **[High] Password exposed on the CLI process list.** Removed `-W/--password` from the CLI entirely — passwords are never accepted on the command line (so not visible via `/proc`/`ps`). Password now comes from `ORACLE_PASSWORD` env, a `PASSWORD=` connect-string component, or an interactive echo-off prompt (`rpassword`) fired in `main()` when a user is set but no password is provided.
- [x] **[High] Plaintext passwords persisted.** Dropped password persistence: frog only ever reads config, never writes it; CLI/connect-string-derived `saved_connections` entries are scrubbed to `password: None`; the connection-dialog password is ephemeral in-memory session state (needed only to connect) and is not stored to disk.
- [x] **[Medium] `frog_history.txt` stores SQL in plaintext near the binary** — now created with owner-only (0o600) permissions on Unix. `max_history` is now enforced in memory. Consider an on-disk size cap as well.
- [x] **[Low] `@file` can execute any readable file with no prompting** — now prompts `Run '…'? [y/N]` in the command bar for files with a non-`.sql` extension.

### Error Handling & UX

- [x] **[Medium] Config parse/read errors are silently swallowed.** `load_config_file` now prints a warning to stderr for explicit `-f` misconfig (missing / unreadable / unparseable) instead of silently falling back to defaults.
- [x] **[Low] Pagination error UX.** A paged fetch that errors now surfaces as a status-line message instead of appending a duplicate/error row on top of existing data.
- [x] **[Low] `status_message` is never auto-cleared.** Added `status_message_ttl` (5s) + `tick_statuses()` called every frame; stale notices auto-expire.

### Quality / Housekeeping

- [x] **[Low] Fix clippy warnings.** Cleaned all clippy warnings to zero (complex-type alias `QueryData`, `fetch_error.unwrap()` refactor, `manual_clamp`, `needless_return`, single-char `push_str`, unused `mut`, etc.).
- [x] **[Low] `render_table` takes 8 args.** Grouped scroll/format/focus into a `TableViewState` struct.
- [x] **[Low] Add integration/unit tests for the new SQL-aware statement splitter.** Added tests: string literals, comments, PL/SQL blocks, multi-statement splits (+ existing `parse_file_directive` and `expand_statements` tests).
- [ ] **[Low] Consider `oracle-rs` pure-Rust driver** once it matures (currently async-only, incompatible).
- [x] **[Low] No `LD_LIBRARY_PATH`/ODPI-C runtime check.** Connect errors containing `DPI-1047` / `libclntsh` now show a friendly hint (install Instant Client + set `LD_LIBRARY_PATH`) instead of a bare error.

## Notes

- History file format: entries separated by `-- frog@YYYY-MM-DD HH:MM:SS;` comment markers, each statement terminated with `;`.
- Oracle Instant Client required (ODPI-C). Set `LD_LIBRARY_PATH` if `DPI-1047` occurs.
