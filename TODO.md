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
./target/release/frog -H localhost -P 1521 -S ORCL -U user -W pass

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

## TODO / Known Issues

> Audited as of Aug 2026. Items grouped by focus; priority: High / Medium / Low.

### Correctness & SQL Parsing

- [ ] **[High] Statement splitter is not SQL-aware.** `SqlEditor::statements_with_ranges` splits on every `;` and `/`-only line with no awareness of string literals (`'a;b'`), inline/`/* */` comments, or PL/SQL blocks (`BEGIN ... END;`). A `.sql` file containing these will be mis-split and produce broken statements. Need a small parser that tracks quotes/comments and handles PL/SQL blocks (with trailing `/` line).
- [ ] **[Medium] `@file` expansion has no dotted-identifier / missing-file hint** — a mistyped `@file` currently aborts with only the raw read error; consider surfacing the attempted path and offering `@@` sibling resolution hint (partially done).

### Concurrency & Resource Lifecycle

- [ ] **[High] Oracle connection leaks on session close.** `remove_session` drops the `Session` but never removes the `Arc<OracleConnection>` from `conn_map` nor calls `OracleConnection::close()`. Long sessions accumulate live DB connections until app exit.
- [ ] **[High] Unbounded in-memory growth.** `session.results` grows for every executed statement/script and is never cleared — long sessions accumulate rows. `session.query_history` is also unbounded in-memory even though `max_history` config exists (never enforced).
- [ ] **[Medium] One OS thread per execution.** Each `execute_query` / `execute_script` / `fetch_next_page` spawns a raw `thread::spawn` with no cap; rapid `Ctrl+Enter` can pile up threads and interleave results. Consider a per-session worker with a job queue, or at least track an in-flight job id so stale results are ignored.
- [ ] **[Medium] Cancel/execute race on shared connection.** `cancel_query` calls `break_execution` on a connection that a worker thread may be executing against concurrently. Works in practice, but there is no synchronization guaranteeing the break targets the in-flight statement — document or serialize.

### Configuration Wiring / Dead Code

- [ ] **[High] `autocommit` config is unused.** `Config.autocommit` defaults to `true` but nothing ever calls `oracle`'s `set_autocommit`; connections silently start with autocommit **disabled** (crate default). Implies UI says one thing and behavior is another — DML is not committed automatically. Decide semantics and wire it in `connect_active`/`add_session`.
- [ ] **[Medium] `max_rows`/`FROG_MAX_ROWS` is ignored.** Parsed in CLI/config but execution uses hard-coded `Session.page_size = 100`; the prefetched page size never reads `max_rows`.
- [ ] **[Low] `UiConfig` (theme, tab_size, date_format, null_display) is dead config.** Parsed and defaulted but never applied to rendering.
- [ ] **[Low] `Config::connect_string(password)` takes a `password` arg it never uses** — misleading signature; remove the parameter.

### Security & Secrets

- [ ] **[High] Password exposed on the CLI process list.** `-W/--password` and `PASSWORD=` inside a `-d` connect string are visible to other users via `/proc`/`ps` during startup. Recommend prompting for a missing password (or env-only / keyring), and scrubbing `PASSWORD=` from any persisted `saved_connections`.
- [ ] **[High] Plaintext passwords persisted.** `connections[]` / `saved_connections` store passwords in `~/.config/frog/config.yml` and in `ConnectionDialog.password` with no masking/encryption. Document, restrict file perms, or integrate a keyring (`secret-service`/`keyring` crate).
- [ ] **[Medium] `frog_history.txt` stores SQL in plaintext near the binary** with default perms; may capture sensitive statements (e.g., `ALTER USER ... IDENTIFIED BY '...'`). Consider honoring `max_history`, a size cap, and 0600 perms.
- [ ] **[Low] `@file` can execute any readable file with no prompting** — intended feature, but note there is no confirmation for non-`.sql` files.

### Error Handling & UX

- [ ] **[Medium] Config parse/read errors are silently swallowed.** `load_config_file` returns `None` on any failure and the app falls back to defaults with no message — silent misconfiguration. Surface parse errors to the status bar / startup.
- [ ] **[Low] Pagination error UX.** A paged fetch that errors appends a separate error row while prior success rows remain visible; consider a status-line message instead (partially addressed for cancel).
- [ ] **[Low] `status_message` is never auto-cleared** — a stale notice can persist indefinitely. Consider auto-expire after N seconds.

### Quality / Housekeeping

- [ ] **[Low] Fix clippy warnings (26 bin + test).** Key ones: `too_many_arguments` on `render_table`, `manual_clamp`, `needless_return` in `handle_conn_dialog`, `single_char` `push_str` loops, and the `fetch_error.unwrap()` after `is_some()` pattern in `connection.rs` (replace with `Result` refactor).
- [ ] **[Low] `render_table` takes 8 args** — group scroll/format state into a small struct.
- [ ] **[Low] Add integration/unit tests** for the new SQL-aware statement splitter once implemented (current splitter has none).
- [ ] **[Low] Consider `oracle-rs` pure-Rust driver** once it matures (currently async-only, incompatible).
- [ ] **[Low] No `LD_LIBRARY_PATH`/ODPI-C runtime check** — crashes with `DPI-1047` on start; give a friendly error instead of an unwrap-style failure.

## Notes

- History file format: entries separated by `-- frog@YYYY-MM-DD HH:MM:SS;` comment markers, each statement terminated with `;`.
- Oracle Instant Client required (ODPI-C). Set `LD_LIBRARY_PATH` if `DPI-1047` occurs.
