# Frog

A terminal-based Oracle database client written in Rust — inspired by Toad, tmux, btop, and Hyprland.

Frog gives you a fast, keyboard-driven TUI for querying Oracle, with multi-session tabs, an embedded SQL editor, pretty result tables, and a sqlplus-style scripting workflow.

## Features

- **Multi-session tabs** — tmux-style windows with hover-to-focus and per-tab connections.
- **Embedded SQL editor** — real cursor movement, line numbers, statement-at-cursor highlighting, vertical scroll.
- **Result viewer** — pretty-printed tables (Table / Markdown / ASCII), 2D scrolling, clipboard copy, pagination.
- **sqlplus-style scripting** — run a `.sql` file with `@file.sql`; support nested includes with `@@rel/path.sql`.
- **Abort on error** — script runs stop at the first failing statement with a clear error log.
- **Background query execution** — queries run on a worker thread; kick a running query with `Ctrl+C`/`Esc`.
- **SQL history** — persisted to `frog_history.txt` next to the binary and browsable in-app.
- **Oracle session overview** — `v$session` viewer.

## Requirements

- Rust toolchain (edition 2021).
- Oracle Instant Client (`libclntsh.so`) — the binary links against ODPI-C. On Arch install via AUR: `oracle-instantclient-basic`, then set `LD_LIBRARY_PATH` if you hit `DPI-1047`.

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
export ORACLE_HOST=localhost ORACLE_PORT=1521 ORACLE_SERVICE=ORCL \
       ORACLE_USER=u ORACLE_PASSWORD=p
./target/release/frog

# Or a connect string
./target/release/frog -d 'host=localhost;port=1521;service_name=ORCL;user=scott;password=tiger'
```

All connection parameters can be supplied via CLI flags, environment variables, or the in-app connection dialog (`Ctrl+O`). Optional YAML config lives at `~/.config/frog/config.yml`.

## Keyboard Reference

| Key | Action |
|---|---|
| `Ctrl+Enter` | Execute statement at cursor |
| `F5` | Run all statements as script |
| `@file.sql` | Run a `.sql` file as a script (nested `@@` includes) |
| `Tab` | Cycle focus: Editor / Results / Sidebar |
| `Ctrl+M` | Maximize/restore focused panel |
| `Ctrl+B` | Toggle sidebar |
| `Ctrl+Y` | Copy results to clipboard |
| `Ctrl+D` | Toggle Markdown table format |
| `Ctrl+O` | Connection dialog |
| `Ctrl+C` / `Esc` | Cancel running query |
| `Ctrl+T` / `Ctrl+W` | New / close session tab |
| `Ctrl+Left/Right` | Switch session tabs |
| `F1` / `F2` / `F3` | Help / v$session / History |
| `Ctrl+Q` | Quit |
| Mouse | Hover tabs to focus, drag splitter to resize, click to focus panel, scroll results |

## Configuration

Optional `~/.config/frog/config.yml`:

```yaml
connections:
  - name: prod
    host: db.example.com
    port: 1521
    service: ORCL
    user: app
    password: secret
defaults:
  max_rows: 10000
  autocommit: true
  max_history: 1000
ui:
  tab_size: 4
  date_format: "%Y-%m-%d %H:%M:%S"
  null_display: "(NULL)"
```

## Project Structure

```
src/
  main.rs              # Entry point
  cli/mod.rs           # CLI args + env vars + YAML config
  db/
    mod.rs             # Re-exports
    connection.rs      # OracleConnection wrapper (oracle crate / ODPI-C)
    session_manager.rs # Sessions, SqlEditor, SessionManager, history, @file expansion
  tui/
    mod.rs             # Re-exports App
    app.rs             # Event loop, key/mouse handling, layout, focus management
    widgets.rs         # Tabs, status bar, table viewer, help, dialogs, panels
```

## Scripting

Frog supports sqlplus-style script files directly from the editor:

```sql
@setup.sql
```

- `@file.sql` — run the file, resolving paths from the current working directory.
- `@@dir/file.sql` — nested include, resolved relative to the including file.
- Scripts stop at the first failing statement, printing which statement and the failing SQL.

## Testing

```bash
cargo test
```

## Known Issues

- No horizontal scrolling for long editor lines.
- Result pagination is capped at an initial page (use `Ctrl+F` to fetch more).
- `connections[]` config entries are not yet wired into the connection-dialog picker.
