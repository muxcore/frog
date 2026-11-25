# Changelog

All notable changes to frog are documented here.

## [0.10.0] - 2026-11-25

### Added
- **Emacs-style incremental search (`Ctrl+S`)**: I-search in the current
  view — SQL editor, result table, `F12` explorer (tree + detail
  preview/DDL), `F2` sessions (list/SQL/plan), `F4` DDL, `F6` perf
  (queries/results/SQL), history and help. Type to jump live,
  `Ctrl+S` / `Ctrl+R` steps next/previous (with wrap notice), `Enter` /
  `Esc` keeps the match, `Ctrl+G` aborts back to the pre-search cursor.
  Smart-case (all-lowercase queries match case-insensitively), editor
  substring and result-cell highlighting, paste lands in the query.

## [0.9.0] - 2026-11-24

### Added
- **Toad-style `F4` Object Describe / DDL viewer**: press `F4` with the cursor
  on any object name in the SQL editor (table, index, view, materialized view,
  sequence, package, procedure, function, trigger, type, or synonym) — or on
  any object in the `F12` DB explorer — to inspect its resolved metadata and
  full DDL script in a syntax-highlighted viewer.
  - Context-aware SQL target extraction: resolves `schema.object`, aliases and
    `alias.column` / `table.column` references back to their base relation,
    Oracle optimizer hints (`/*+ INDEX(e idx_name) */`), `package.member`
    calls, private/`PUBLIC` synonyms (emitting both the `CREATE SYNONYM` and
    underlying object DDL), and `search_path` objects on Postgres.
  - Rich Toad-grade table & object DDL on both Oracle and Postgres: column-aligned
    `CREATE TABLE` with exact lengths/precision/scale, `DEFAULT` / `IDENTITY`
    clauses, `PRIMARY KEY` / `UNIQUE` / `FOREIGN KEY` / `CHECK` constraints,
    partition keys, secondary/bitmap/function-based `CREATE INDEX` statements,
    table/column `COMMENT`s, full `PACKAGE` + `PACKAGE BODY` source, and all
    overloads for Postgres routines.
  - Actions inside `F4`: `Ctrl+C` / `Ctrl+Y` / `c` copies the DDL, `Enter` /
    `i` loads it into the SQL editor, `n` opens it in a new session tab, `r`
    reloads from the catalog, `Alt+M` (`Mouse:OFF`) hides borders and line
    numbers for clean terminal drag-copy, and `F4` / `Esc` returns to your
    prior view (`Query` or `F12` Explorer).
- **`F6` Performance & DBA Dashboard (`btop` + Toad style)**: dedicated
  database health and diagnostics view with lightweight terminal graphics for
  both Oracle and Postgres:
  - Top 4-card live telemetry strip with rolling `Sparkline`s (`▁▂▃▅▇`),
    sub-character smooth Unicode fractional bars (`█████▋░░`), active/total
    session load, blocking lock alerts, proportional multi-color CPU/Wait
    Class stacked bars, and SGA+PGA / Buffer Cache Hit gauges.
  - 10 curated DBA diagnostic queries per backend: blocking & waiting locks,
    top CPU consuming sessions, top SQL by CPU/elapsed time (`v$sql` /
    `pg_stat_statements` with automatic live-activity fallback), memory
    overview (SGA/PGA & `pg_settings`/buffer cache), top PGA memory sessions /
    idle-in-transaction sessions, wait event bottlenecks, long operations /
    storage & tablespace usage (with `USER_*` privilege fallback), I/O heavy
    SQL / sequential scan analysis, and active transactions / dead-tuple
    autovacuum health.
  - Inline high-resolution Unicode bar graphics inside percentage and resource
    metric cells (`SHARE_PCT`, `USED_PCT`, `CACHE_HIT_PCT`, `CPU_SEC`,
    `PGA_MB`, `WAIT_SECS`, …).
  - Controls inside `F6`: `1`–`0` or `↑`/`↓` picks a query (rapid cursor moves
    skip superseded DB executions), `Enter`/`r` runs, `R`/`F5` refreshes
    telemetry + query, `a` toggles 5-second live `btop` auto-refresh, `v`
    toggles the underlying diagnostic SQL preview, `s`/`i` sends the SQL to
    the editor, `n` opens it in a new session tab, `Tab` switches panes, and
    `Ctrl+Z`/`F11` zooms the focused pane.

## [0.8.2] - 2026-11-23

### Added
- Layered clipboard copy (drag-release, `Ctrl+C`, `Ctrl+Y`, `Alt+Y`):
  arboard first, then an external helper when the OS clipboard is not
  reachable — `win32yank.exe` / `clip.exe` on WSL/Windows, `wl-copy` /
  `xclip` / `xsel` / `pbcopy` on desktops — and OSC52 for SSH. A custom
  bridge can be plugged in with `FROG_CLIPBOARD_CMD` (env or `.env`) or
  `defaults.clipboard_command` in `config.yml` (text on stdin). The
  status line now names the backend that worked.
- Clearer copy hint over SSH: when only OSC52 was available, point at
  Shift+drag (the terminal's own copy-on-select) and at mintty's
  `AllowSetSelection` / Git Bash's `ssh` (Windows `ssh.exe` blocks OSC52).

### Fixed
- The OS clipboard handle is now created once and kept for the whole
  session. On X11/Wayland the clipboard content is served by a background
  thread owned by that handle, so creating it, setting the text and
  dropping it immediately could leave the clipboard empty (unless a
  clipboard manager grabbed the text first). Pasting and middle-click
  reads reuse the same handle instead of reconnecting.
- Pasting into the result table over git-bash/mintty (or any terminal or
  proxy that doesn't emit bracketed paste): a paste arrives as a burst of
  key events, and those were handled one by one, so text was silently
  dropped while the table/sidebar had focus, pasted tabs cycled focus and
  pasted newlines could run a statement. Bursts are now reassembled into a
  single paste, and typing or pasting while a read-only pane is focused
  moves focus to the SQL editor.

## [0.8.1] - 2026-11-22

### Fixed
- Terminal recovery on panic or early startup failure: an RAII terminal
  guard and panic hook now unconditionally restore raw mode, alternate
  screen, mouse capture, bracketed paste, and Kitty keyboard flags.
- Connection lifecycle & race safety: background connects now carry a
  per-session `conn_seq` token and register in `conn_map` on the main
  loop, closing superseded, replaced, or post-tab-close DB handles
  promptly so sessions never leak or overwrite a newer connection.
- Thread-safe `F2` / `F12` catalog queries: synchronous UI queries now
  fail fast while a user query is running and serialize on `exec_lock`
  (with mutex poison recovery), preventing concurrent ODPI-C calls and
  UI freezes.
- Multi-statement script state (`F5`): `pending_query` stays active
  across all statements in a script (keeping the `RUNNING` indicator and
  `Esc`/`Ctrl+C` cancellation working past statement 1) and drains up to
  32 results per frame.
- Oracle pagination & `RN` column preservation: `WITH` CTE pagination no
  longer strips real user columns named `RN` on page 2+, recognizes
  multiline `WITH` headers, uses saturating bounds, and renames the
  internal `ROWNUM` helper column to `FROG_RN__` to avoid `ORA-00918`.
- Safe pagination tracking & transaction commands: `INSERT`/`UPDATE`/
  `DELETE ... RETURNING`, `EXPLAIN`, and `SHOW` no longer overwrite
  `last_sql` for scroll pagination; `COMMIT`/`ROLLBACK` now tolerate
  leading SQL comments and trailing `;`.
- SQL splitter & UTF-8 cursor positioning: `statement_ranges` now
  respects Oracle `q'[...]'` alternative quotes and Postgres `$$...$$` /
  `$tag$...$tag$` dollar-quoted blocks/functions; fixed editor terminal
  cursor display-width alignment on multi-byte characters, clamped
  `ConnectionDialog::cursor_right`, and aligned masked password cursors
  by character index.

## [0.8.0] - 2026-11-20

### Fixed
- Editor crash on multi-byte border glyphs: vertical cursor move onto a
  line starting with `│` left the byte column mid-char, then `Delete` /
  `Backspace` panicked (`byte index 1 is not a char boundary`). All
  cursor setters now snap to char boundaries; added regression test.
- Result table string handling: column widths are content-based
  (header-only widths truncated long strings), embedded newlines/tabs
  in values are collapsed to single-line cells, Markdown export escapes
  pipes.
- Result scrolling parity across formats: Markdown/ASCII now use the
  same responsive column window as Table (no more fixed 10-col slice),
  correct per-format pane sizing, `▲▼◀▶` + scrollbars in every title,
  and scroll offsets are clamped into the loaded data each frame so
  overscrolling past the end can't wedge the opposite direction in any
  format. Fresh results also reset the column offset.
- Fixed paste-after-copy over SSH: the copy rides OSC52 to the *local*
  terminal but arboard only reads the *remote* clipboard, so a table
  just copied could never be pasted back. Every in-app copy is now also
  remembered internally and used when the OS clipboard is unavailable.

### Added
- Mouse:OFF native-copy mode renders the editor borderless with no
  gutter, so terminal drag-select copies clean SQL without `│`/`─`
  border junk (`Alt+M` restores).
- Explicit keyboard paste: `Ctrl+V` / `Shift+Ins` pastes the OS
  clipboard via the bracketed-paste path in editor, dialog and command
  bar, with capture ON or OFF; `Shift+wheel` scrolls result columns.
- Responsive table on big monitors: sidebar share is capped as the
  terminal widens (25% → 20/15/12%), so extra width goes to result
  columns; the column viewport fits the pane instead of a fixed 10-col
  slice, with leftover space distributed across columns. Short screens
  shrink the editor so results keep ~8 rows.
- Overflow signals beyond counts: `◀`/`▶` and `▲`/`▼` arrows in the
  result title plus vertical/horizontal scrollbars.
- In-editor drag selection with mouse capture ON: dragging across SQL
  text highlights it and auto-copies to the clipboard on release (OS +
  OSC52, so it works over SSH with no terminal-native selection or
  Mouse:OFF toggle needed). `Ctrl+C` copies the active selection when
  no query is running, typing/pasting replaces the selection, `Esc`
  clears it.
- `Ctrl+C` is now the standard copy key when no query is running (cancel
  still wins mid-query): same contextual copy as `Ctrl+Y` — selection
  first, else the statement/table the focused pane shows. `Ctrl+V`
  pastes the OS clipboard, falling back to frog's own copy buffer.
- Header-row selection: dragging from the header selects column names
  (highlighted in place); every table copy automatically includes the
  header row. The title shows the range (`[sel hdr c1-3]`,
  `[sel hdr-5 c2-4]`).
- `Ctrl+Y` is now contextual: in Results it copies the table
  drag-selection (or the full table), in the Editor the drag-selection
  (or the SQL statement at the cursor) — one copy key driven by focus.
  `Alt+Y` remains as the explicit copy-SQL key.
- Table drag-selection in every result format (Table, Markdown, ASCII):
  dragging highlights exactly the selected cells and copies on release
  in the view's own format — TSV (header + rows, no borders) in Table,
  Markdown with its `|` vertical lines kept in Markdown, the ASCII grid
  in ASCII. Works with mouse capture ON over SSH (OS clipboard + OSC52);
  `Ctrl+C` copies it when no query runs, `Esc` clears it, and the title
  shows the selected range (`[sel r3-5 c2-4]`).
- ARM builds: `scripts/build-old-glibc.sh` takes an arch (`x86_64` /
  `aarch64`, host arch by default) and produces `dist/frog-<arch>`;
  `scripts/release.sh` builds and publishes both (`frog-x86_64.gz`,
  `frog-aarch64.gz`), single-arch via `FROG_ARCHES="x86_64"`.

## [0.5.8] - 2026-11-13

### Added
- Toad-style SQL completion popup in the editor (`Ctrl+Space`, `Ctrl+.`
  fallback): alias → columns, schema → objects, table → columns, plus
  keywords/functions and identifiers already in the statement. `↑`/`↓`/
  `PgUp`/`PgDn` select, `Enter`/`Tab` insert, `Esc` closes.
- Background completion metadata: cached per session (schemas → object names
  → columns of referenced tables) with TTL, stale-while-revalidate and DDL
  invalidation. Keystrokes only read memory; catalog queries never run on the
  UI thread. Oracle uses `USER_TAB_COLUMNS` for the current schema and batched
  `ALL_TAB_COLUMNS` for others; Postgres resolves refs with `to_regclass` and
  fetches all requested tables in one `unnest … WITH ORDINALITY` query.
- `docs/sql-completion.md`: research notes, design and phased plan.

## [0.5.7] - 2026-10-22

### Added
- F12 DB explorer pane controls: `Tab` switches focus between the
  Objects tree and the Detail pane, `Ctrl+Z`/`F11` zooms the focused
  pane, and `Alt+Up`/`Down` resizes the split (mouse click focuses too).
- F12 detail pane scrolls like the result viewer/editor: `Up`/`Down`
  (or `j`/`k`) and `PgUp`/`PgDn` scroll rows/text, `Left`/`Right`
  (`h`/`l`) scroll preview columns, `Home` resets, wheel scrolls and
  `Shift+wheel`/horizontal wheel scrolls preview columns.
- Pagination for F12 previews: scrolling into the last few rows appends
  the next 20-row page in place (`Ctrl+F` fetches one explicitly), on
  both Oracle and Postgres. The main result viewer now also auto-fetches
  its next page when scrolling near the end, keeping the existing rows
  visible while the page loads.
- F2 session browser: the Current SQL pane now scrolls (wrapped) instead
  of clipping a long statement, and the Explain plan pane scrolls both
  vertically and horizontally. Scrolling follows the focused pane
  (`Tab` to switch): `↑`/`↓`/`PgUp`/`PgDn`, `Home`/`End` for top/bottom,
  `←`/`→` for plan sideways, and the mouse wheel scrolls the pane under
  the pointer (Shift+wheel scrolls the plan sideways).

## [0.5.6] - 2026-10-16

### Added
- F2 session viewer: `Alt+Up`/`Down` now grows/shrinks the focused
  pane, so `Current SQL` (and the explain plan) can be extended
  directly with the same keys as the query tab — `Tab` to focus,
  `Alt+Up` to grow, `Ctrl+Z`/`F11` to zoom. `Ctrl+Up`/`Down` still
  sizes SQL directly; pane titles and the status bar show the hints.
- `scripts/release.sh`: one-command old-glibc build + GitHub release
  refresh — calls `scripts/build-old-glibc.sh`, gzips `dist/frog`
  and creates (or refreshes) the version release with the gzipped
  binary as asset.

## [0.5.5] - 2026-10-15

### Added
- F2 session viewer pane layout matches the query tab: `Tab` cycles
  focus List/SQL/Plan, `Ctrl+Z`/`F11` maximizes the focused pane,
  `Alt+Up`/`Down` grows/shrinks the focused pane (so List, Current
  SQL and Plan can each be extended), `Ctrl+Up`/`Down` sizes SQL
  directly, mouse click focuses, drag on a horizontal splitter
  resizes, wheel scrolls list/plan.

## [0.5.0] - 2026-10-10

### Added
- `Alt+Y` copies the SQL statement at cursor to clipboard (built from
  the editor buffer, so line numbers can never leak in).
- Clipboard OSC52 fallback: `Ctrl+Y` / `Alt+Y` try the OS clipboard
  first, then OSC52, so copy lands on the local clipboard over SSH /
  headless (tmux needs `set -g set-clipboard on`, mintty needs
  `AllowSetSelection=true`).
- `Alt+M` toggles mouse capture (plain `ESC m` survives
  git-bash/mintty and jump-host proxies where `Ctrl+M` arrives as
  Enter). While `OFF` the editor gutter is hidden so a native
  drag-select copies clean SQL; status bar and editor title show the
  state.
- Paste hardening for git-bash/mintty (no bracketed paste): input
  queue is drained each frame so multi-KB pastes land quickly;
  `CRLF`/`CR`-only breaks normalized, pasted tabs expand to spaces.

### Fixed
- Kitty keyboard enhancements are only requested when the terminal
  reports support.
- Middle-click paste over SSH with no X display hints at
  `Shift+Ins` / `Ctrl+Shift+V` instead of failing silently.

## [0.4.0] - 2026-10-05

### Added
- F2 session browser: `a` toggles the ACTIVE-only filter (press again to
  show all), `s` cycles the sort column across all 8 visible columns
  (then back to DB order), `S` toggles sort direction. The header shows
  ▲/▼ on the sorted column and the title bar shows filter/sort state.

### Docs
- Keybinding docs synced for consistency across README, in-app help
  (F1), editor title and session browser title: execute statement is
  `Ctrl+Enter / Alt+Enter / Ctrl+J / F8 / F9`, run script is
  `F5 / Ctrl+R`, `Ctrl+D` cycles Table → Markdown → ASCII, session
  browser lists `Enter`/`e`, `r`, `R`/`F5`, `a`, `s`/`S` everywhere.

## [0.3.1] - 2026-10-02

### Added
- Configurable F2 session browser auto-refresh: `--session-refresh-secs`
  / `FROG_SESSION_REFRESH_SECS` (`FROG_SESSION_REFRESH` alias) / `.env`
  (same keys) / `config.yml` `defaults.session_refresh_secs`
  (`session_refresh` alias). Default 60s, `0` disables auto-refresh.
- Manual F2 list refresh with `R` / `F5` (`r` still reloads the row SQL);
  title bar shows interval and age (e.g. `auto 60s · 12s ago · R refresh`).
- Editor title now shows `Ctrl+Enter/F9` for running a statement;
  help lists `Ctrl+Enter / Alt+Enter / F8 / F9` (some terminals swallow
  `Ctrl+Enter`).

### Fixed
- F2 session list is now cached with TTL and refreshed on open / interval /
  manual key instead of querying `v$session` / `pg_stat_activity` on every
  UI frame and cursor move.

## [0.3.0] - 2026-09-27

- Added F12 database navigator (schema → type folders, 20-row preview,
  DDL/source detail).
- Version bump to 0.3.0.

## [0.2.0] - 2026-09-26

- Postgres alpha support (`--db-type postgres`, `PG*` env vars,
  `postgres://` URLs, `pg_stat_activity` browser, `LIMIT`/`OFFSET` paging).
- Per-setting precedence fix: CLI flag > env var > `.env` > default;
  connect descriptor belongs to its backend. Numeric parsing fixes.

## [0.1.0] - 2026-08-01

- Initial Oracle terminal client (tabs, SQL editor, result viewer,
  background execution, `v$session` browser, connection dialog).
