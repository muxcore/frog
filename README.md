# Frog

You ocassianly need to ssh to remote machine and debug some stuck session. You absolutley hate sqlplus
visual (functionality is fine), I've got you:

Meet 'Frog' - A terminal-based Oracle + Postgres database client written in Rust — name inspired by Toad. Tmux, btop for UI.

**Before you start reading further: If you are against AI assisted developement, look away. Otherwise continue.**

## Features

- **Multi-session tabs** — tmux-style windows with hover-to-focus and per-tab connections.
- **Embedded SQL editor** — real cursor movement, line numbers, statement-at-cursor highlighting, vertical scroll.
- **Result viewer** — pretty-printed tables (Table / Markdown / ASCII), 2D scrolling, clipboard copy, pagination.
- **(experimental)sqlplus-style scripting** — run a `.sql` file with `@file.sql`; support nested includes with `@@rel/path.sql`.
- **Background query execution** — queries run on a worker thread; kick a running query with `Ctrl+C`/`Esc`.
- **SQL history** — persisted to `frog_history.txt` next to the binary and browsable in-app.
- **Oracle + Postgres** — pick the backend with `--db-type` or the `Type:` row in the connection dialog (`Ctrl+O`). Postgres needs no client libraries.
- **Session browser (F2)** — `v$session` / `pg_stat_activity` viewer. Pick a row to see its current SQL, `Enter` shows the explain plan (TOAD-style).
- **`.env` support** — connection settings are also read from a `.env` file in the working directory (real environment variables take priority).

![Demo GIF](multimedia/demo.gif)


## Requirements

- Rust toolchain (edition 2021).
- For Oracle: Oracle Instant Client (`libclntsh.so`) — the binary links against ODPI-C. On Arch install via AUR: `oracle-instantclient-basic`, then set `LD_LIBRARY_PATH` if you hit `DPI-1047`.
- For Postgres: nothing extra (pure-Rust driver, plaintext `NoTls` in v1).

## Build

```bash
cargo build --release
# Binary: ./target/release/frog
```

## Building for older glibc (containers)

### Why this is sometimes needed

A Rust binary is linked against the glibc of the machine that **built** it. If you
build on a distro with a newer glibc (e.g. glibc 2.43) and copy the binary to a
server with an older glibc (e.g. glibc 2.34), the loader aborts at startup with
something like:

```
version `GLIBC_2.39' not found (required by ./frog)
```

In frog's case the two extra symbol versions come from:

- `__isoc23_strtol` (needs glibc 2.38) — pulled in by compiling ODPI-C's `dpi.c`
  (`odpic-sys` uses `cc`) against current headers.
- `pidfd_getpid` / `pidfd_spawnp` (need glibc 2.39) — referenced by recent Rust `std`.

Since glibc symbol versions can't be retrofitted, the binary must be rebuilt on a
glibc that is **older than or equal to** the target server's. The scripts below do
exactly that inside a container based on Ubuntu 20.04 (glibc 2.31), which produces
a binary whose highest required symbol is `GLIBC_2.30` or lower — runnable on
glibc 2.34 servers.

> **Why not a fully static build?** ODPI-C `dlopen`s Oracle's `libclntsh.so` at
> runtime, and Oracle Instant Client only ships glibc-linked shared libraries. A
> fully static (musl) process cannot `dlopen` a glibc-linked `.so`, so a static
> frog would start but fail to connect (`DPI-1047`). A container build using an
> old glibc keeps normal dynamic linking, so `libclntsh.so` loads fine on the
> target server.
>
> Latest Oracle Instant Client (23) needs glibc ≥ 2.28, which the glibc-2.34
> server already satisfies, so using it doesn't raise the requirement.

### Usage

Requires [Docker](https://docs.docker.com/get-docker/).

```bash
# Build dist/frog (glibc <= 2.34 compatible)
./scripts/build-old-glibc.sh

# Remove everything the build created (image, volumes, dist/)
./scripts/clean-old-glibc.sh
```

`build-old-glibc.sh` builds the binary, verifies with `objdump` that the highest
required GLIBC symbol is ≤ `2.34` (fail-fast otherwise), and writes the result to
`dist/frog`. The produced binary is still dynamically linked, so the server still
needs Oracle Instant Client (`libclntsh.so`, via `LD_LIBRARY_PATH`) and `libgcc_s`.

The scripts are opinionated (base image, target glibc, Rust toolchain) but easy to
tune — see the variables at the top of each script and `scripts/Dockerfile.oldglibc`.

## Run

```bash
# Oracle: CLI args
./target/release/frog -H localhost -P 1521 -S ORCL -U scott

# Oracle: env vars
export ORACLE_HOST=localhost ORACLE_PORT=1521 ORACLE_SERVICE=ORCL \
       ORACLE_USER=scott ORACLE_PASSWORD=tiger
./target/release/frog

# Oracle: connect string
./target/release/frog -d 'host=localhost;port=1521;service_name=ORCL;user=scott'

# Postgres: URL connect string
./target/release/frog -d 'postgres://scott:tiger@db1.local:5432/myapp'

# Postgres: CLI args
./target/release/frog --db-type postgres -H db1.local -D myapp -U scott

# Postgres: standard PG* env vars (frog picks postgres automatically
# when only PG* variables are set)
export FROG_DB_TYPE=postgres PGHOST=db1.local PGDATABASE=myapp \
       PGUSER=scott PGPASSWORD=tiger
./target/release/frog
```

All connection parameters can be supplied via CLI flags, environment variables, or the in-app connection dialog (`Ctrl+O`). Optional YAML config lives at `~/.config/frog/config.yml`.

### `.env` file

If a `.env` file exists in the directory frog is started from, its variables are used as a second-priority source — after real environment variables. Precedence per setting:

```
CLI flag  >  environment variable (ORACLE_* / PG* / FROG_* / DATABASE_URL)  >  .env  >  built-in default
```

Precedence is **per setting**: individual env vars/flags always beat the same field inside a connect string, no matter where the connect string came from. So real `PGHOST` beats `HOST=` inside a `.env` `ORACLE_CONNECT`. A connect descriptor also belongs to its backend: overriding `--db-type` drops the descriptor's host/port (names like service/database are kept, since `-S` doubles as a dbname alias).

Example `.env` (Oracle):

```bash
ORACLE_HOST=db1.local
ORACLE_PORT=1521
ORACLE_SERVICE=ORCL
ORACLE_USER=scott
ORACLE_PASSWORD=tiger
# or a full connect string instead of the individual fields:
# ORACLE_CONNECT=HOST=db1.local;PORT=1521;SERVICE_NAME=ORCL;USER=scott;PASSWORD=tiger
```

Example `.env` (Postgres):

```bash
FROG_DB_TYPE=postgres
PGHOST=db1.local
PGPORT=5432
PGDATABASE=myapp
PGUSER=scott
PGPASSWORD=tiger
# or a URL instead of the individual fields:
# ORACLE_CONNECT=postgres://scott:tiger@db1.local:5432/myapp
# (DATABASE_URL works as a fallback for ORACLE_CONNECT)
```

Supported keys: `ORACLE_CONNECT`, `DATABASE_URL`, `ORACLE_HOST`, `ORACLE_PORT`, `ORACLE_SERVICE`, `ORACLE_USER`, `ORACLE_PASSWORD`, `FROG_DB_TYPE`, `FROG_CONFIG`, `FROG_MAX_ROWS`, `FROG_NO_AUTOCOMMIT`, `FROG_SESSION_REFRESH_SECS` (`FROG_SESSION_REFRESH` accepted as an alias), `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGPASSWORD`. Comments (`#`) and quoted values are handled; keys already present in the environment keep their environment value and are reported as ignored. If `--db-type` is omitted, frog selects postgres when only `PG*` variables are set, otherwise oracle.

> **Dialog shows defaults?** The `.env` is read from the directory frog is *started* in — a `.env` elsewhere is ignored. If nothing configures the connection, frog prints `no connection settings from CLI flags, env vars or ./.env …` at startup and `frog --help` lists every variable it understands.

At startup frog prints one line summarizing what the `.env` contributed, for example:

```
frog: './.env': using ORACLE_USER, ORACLE_PASSWORD; ignored ORACLE_HOST (set in environment)
```

Opening the dialog with `Ctrl+O` **pre-fills its fields from the startup connection parameters** (CLI flags / `ORACLE_*`/`PG*` env vars / `.env` / `-d` connect string / `config.yml`). So if you launch with `ORACLE_HOST`/`ORACLE_SERVICE` set and type the user + password into the dialog, the host/port/service are already there — just fill in credentials and connect. You only need the user present if you want frog to auto-connect on startup.

Inside the dialog: the first row selects the backend (`Type: oracle/postgres` — `Space`/`Left`/`Right` toggles, `o`/`p` picks directly; switching updates the default port `1521`/`5432`). `Tab`/`Up`/`Down` switch fields (the current field is highlighted and shows a visible insertion cursor), `Left`/`Right`/`Home`/`End` move within the text, `Backspace`/`Delete` edit at the cursor, and `Ctrl+U` clears a field (handy for wiping a password that was pre-filled from `.env`). The port field accepts digits only. The third row is labeled `Service:` for Oracle and `Database:` for Postgres (`-S` and `-D`/`DATABASE=` are aliases for each other).

> **Passwords**: never pass a password on the command line (`-W` was removed so it can't leak via `ps`). FROG reads it from `ORACLE_PASSWORD` / `PGPASSWORD` (env or `.env`), a `PASSWORD=` connect-string component (or the `postgres://user:pass@…` URL), or prompts interactively (echo-off) at startup. Frog never persists passwords to disk.

## Keyboard Reference

| Key | Action |
|---|---|
| `Ctrl+Enter` / `Alt+Enter` | Execute statement at cursor |
| `F5` | Run all statements as script |
| `Ctrl+:` | Open command bar (`@file.sql` runs a script, `clear`/`cls` clears results) |
| `Tab` | Cycle focus: Editor / Results / Sidebar |
| `Ctrl+Z` / `F11` | Maximize/restore focused panel |
| `Ctrl+B` | Toggle sidebar |
| `Ctrl+Y` | Copy results to clipboard |
| `Ctrl+D` | Toggle Markdown table format |
| `Ctrl+O` | Connection dialog (backend / host / port / service-or-db / user / password) |
| `Ctrl+C` / `Esc` | Cancel running query |
| `Ctrl+F` | Fetch next page of results |
| `Ctrl+T` / `Ctrl+W` | New / close session tab |
| `Ctrl+Left/Right` | Switch session tabs |
| `F1` / `F2` / `F3` | Help / Session browser (pick row, `Enter` = explain plan, `r` = reload SQL, `R`/`F5` = refresh list) / History |
| `F12` | DB explorer — schema → type folders (tables, views, matviews, indexes, …), 20-row preview in result style (`Ctrl+D` switches format), DDL/source |
| `Ctrl+Q` | Quit |
| `Ctrl+M` | Toggle mouse capture (tmux-style copy/paste mode) |
| Mouse | Hover tabs to focus, drag splitter to resize, click to focus panel, scroll results |

### Copy & paste (tmux style)

- **Middle-click** pastes the primary selection at the clicked position in the editor (falls back to the regular clipboard). Bracketed paste (`Ctrl+Shift+V`) works everywhere — editor, connection dialog and command bar.
- Pressing **`Ctrl+M` turns mouse capture off**: select text with your terminal's native selection to copy it, paste freely, then press `Ctrl+M` again to re-enable frog's mouse handling (scrolling, click-to-focus, splitter drag). The status bar shows `Mouse:OFF` while capture is disabled.
- On most terminals you can also hold **`Shift` while dragging** to use the native selection without toggling capture.
- `Ctrl+Y` remains available to copy the current result set to the system clipboard.

> Like `Ctrl+Enter`, the `Ctrl+M` binding requires a terminal that emits modified-key (`CSI u`) sequences (e.g. Alacritty); otherwise it may arrive as plain Enter.

> **Note on `Ctrl+Enter`:** it isn't an ASCII control character, so it only reaches
> frog as a distinct key when the terminal emulator emits a modified-key (`CSI u`)
> escape sequence for it — which modern Linux terminals like Alacritty do.
> On limited/proxied clients (Git Bash on Windows, or sessions routed through a
> CyberArk PSM / jump-host proxy) `Ctrl+Enter` often arrives as a plain Enter and
> just inserts a newline instead of submitting. If that happens, use **`Alt+Enter`**
> (or `F8`/`F9`), which uses a plain `ESC Enter` sequence that survives any transport.

## Configuration

You are better of using .env, or env variables, but:

Optional `~/.config/frog/config.yml`:

```yaml
connections:
  - name: prod
    host: db.example.com
    port: 1521
    service: ORCL
    user: app
    password: secret
  - name: pg-dev
    db_type: postgres
    host: db1.local
    port: 5432
    database: myapp   # 'service:' works as an alias when 'database:' is omitted
    user: scott
defaults:
  max_rows: 10000
  autocommit: true
  max_history: 1000
  session_refresh_secs: 60  # F2 auto-refresh interval, seconds; 0 = manual (R) only
ui:
  null_display: "(NULL)"
```

## Backend notes

- **Pagination**: Oracle pages with `ROWNUM`/`OFFSET … FETCH`, Postgres with `LIMIT`/`OFFSET` — `Ctrl+F` fetches more on both.
- **Session browser (F2)**: Oracle reads `v$session` (SQL text via `v$sql`), Postgres reads `pg_stat_activity`. `↑`/`↓` picks a row, its SQL loads automatically, `Enter` runs `EXPLAIN` (`EXPLAIN PLAN` + `DBMS_XPLAN` on Oracle, plain `EXPLAIN` — never `ANALYZE` — on Postgres), `r` reloads the SQL, `R`/`F5` refreshes the list, `PgUp`/`PgDn` scroll the plan. Auto-refresh interval (default 60s) via `--session-refresh-secs` / `FROG_SESSION_REFRESH_SECS` (`FROG_SESSION_REFRESH` alias) / `.env` (same) / `config.yml` `defaults.session_refresh_secs` (`session_refresh` alias); `0` disables auto-refresh.
- **F2 privileges (Oracle)**: the browser needs dictionary access. If F2 shows an error about it, ask your DBA for e.g.:
  ```sql
  GRANT SELECT_CATALOG_ROLE TO scott;
  -- or minimally:
  GRANT SELECT ON V_$SESSION TO scott;
  GRANT SELECT ON V_$SQL TO scott;
  ```
- **Postgres statements**: besides `SELECT`/`WITH`, `VALUES`/`TABLE`/`SHOW` and `… RETURNING` run as queries and return rows.




## Scripting
Experimental. Report bugs.  Sqlplus-style script files directly from the editor:

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
