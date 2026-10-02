# Changelog

All notable changes to frog are documented here.

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
