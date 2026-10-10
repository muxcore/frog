# Changelog

All notable changes to frog are documented here.

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
