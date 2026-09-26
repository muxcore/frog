pub mod dotenv;

use crate::db::DbType;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Parser, Clone, Debug)]
#[command(
    name = "frog",
    version,
    about = "Terminal database client (Oracle + Postgres)",
    after_help = "\nExamples:\n\n\
Connect to Oracle with a full connection string (host/port/service/user/password):\n\
    frog -d \"HOST=db1.local;PORT=1521;SERVICE_NAME=ORCL;USER=scott;PASSWORD=tiger\"\n\n\
Connect to Oracle with individual parameters (env variables can be used instead):\n\
    frog -H db1.local -P 1521 -S ORCL -U scott\n\n\
Connect to Postgres with a URL:\n\
    frog -d \"postgres://scott@db1.local:5432/myapp\"\n\n\
Connect to Postgres with individual parameters:\n\
    frog --db-type postgres -H db1.local -D myapp -U scott\n\n\
Same Oracle connection using only environment variables:\n\
    ORACLE_HOST=db1.local ORACLE_SERVICE=ORCL ORACLE_USER=scott ORACLE_PASSWORD=tiger frog\n\n\
Same Postgres connection using standard PG* variables:\n\
    FROG_DB_TYPE=postgres PGHOST=db1.local PGDATABASE=myapp PGUSER=scott frog\n\
\n\
Environment variables (all optional; per setting: CLI flag > env var > .env > default):\n\
A connect string never overrides individual env vars/flags, and a connect\n\
descriptor belongs to its backend (switching backend drops its host/port).\n\
\n\
  Backend selection:\n\
    FROG_DB_TYPE=postgres\n\
        oracle (default) | postgres (pg, postgresql accepted)\n\
        May be omitted when only PG* variables are set (postgres is then picked\n\
        automatically).\n\
\n\
  Oracle (same as -H/-P/-S/-U flags):\n\
    ORACLE_HOST=db1.local ORACLE_PORT=1521 ORACLE_SERVICE=ORCL \\\n\
    ORACLE_USER=scott ORACLE_PASSWORD=tiger\n\
    ORACLE_CONNECT='HOST=db1.local;PORT=1521;SERVICE_NAME=ORCL;USER=scott'\n\
        Full connect string (KEY=VAL, or a postgres://user:pass@host:port/db URL).\n\
        DATABASE_URL is accepted as a fallback for ORACLE_CONNECT.\n\
\n\
  Postgres (standard PG* names):\n\
    PGHOST=db1.local PGPORT=5432 PGDATABASE=myapp PGUSER=scott PGPASSWORD=tiger\n\
\n\
  Other:\n\
    FROG_CONFIG=~/.config/frog/prod.yml  FROG_MAX_ROWS=10000  FROG_NO_AUTOCOMMIT=true\n\
\n\
  A .env file in the startup directory is read as a fallback for all of the above.\n"
)]
pub struct CliArgs {
    #[arg(short = 'H', long, env = "ORACLE_HOST")]
    pub host: Option<String>,

    #[arg(short = 'P', long, env = "ORACLE_PORT")]
    pub port: Option<u16>,

    #[arg(short = 'S', long, env = "ORACLE_SERVICE")]
    pub service: Option<String>,

    /// Postgres database name. `-S` works as an alias when this is omitted.
    #[arg(short = 'D', long, env = "PGDATABASE")]
    pub database: Option<String>,

    /// Backend to use: `oracle` (default) or `postgres` (`pg`/`postgresql` accepted).
    #[arg(long = "db-type", env = "FROG_DB_TYPE")]
    pub db_type: Option<String>,

    #[arg(short = 'U', long = "user", env = "ORACLE_USER")]
    pub user: Option<String>,

    #[arg(short = 'd', long = "connect-string", env = "ORACLE_CONNECT")]
    pub connect_string: Option<String>,

    #[arg(short = 'f', long, env = "FROG_CONFIG")]
    pub config_file: Option<PathBuf>,

    #[arg(long, env = "FROG_MAX_ROWS", default_value = "10000")]
    pub max_rows: usize,

    #[arg(short = 'n', long, env = "FROG_NO_AUTOCOMMIT")]
    pub no_autocommit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub connections: Option<Vec<ConnectionEntry>>,
    pub ui: Option<UiConfig>,
    pub defaults: Option<DefaultsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionEntry {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub service: String,
    pub user: String,
    pub password: Option<String>,
    /// Backend for this entry (`oracle` default). Missing = oracle (backward compat).
    #[serde(default)]
    pub db_type: Option<DbType>,
    /// Postgres database name. When omitted for postgres entries, `service` is used.
    #[serde(default)]
    pub database: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: Option<String>,
    pub tab_size: Option<usize>,
    pub date_format: Option<String>,
    pub null_display: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: Some("dark".into()),
            tab_size: Some(4),
            date_format: Some("%Y-%m-%d %H:%M:%S".into()),
            null_display: Some("(NULL)".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    pub max_rows: Option<usize>,
    pub autocommit: Option<bool>,
    pub max_history: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub connection: ConnectionParams,
    pub ui: UiConfig,
    pub max_rows: usize,
    pub autocommit: bool,
    pub max_history: usize,
    pub saved_connections: Vec<ConnectionEntry>,
}

#[derive(Debug, Clone)]
pub struct ConnectionParams {
    pub db_type: DbType,
    pub host: String,
    pub port: u16,
    /// Oracle service name. For postgres this mirrors `database` when set via `-S`.
    pub service: String,
    /// Postgres database name. For oracle this mirrors `service`.
    pub database: String,
    pub user: String,
    pub password: Option<String>,
}

impl ConnectionParams {
    /// The service/database name for the active backend.
    pub fn db_name(&self) -> &str {
        match self.db_type {
            DbType::Oracle => &self.service,
            DbType::Postgres => &self.database,
        }
    }

    /// Human-readable session label, e.g. `scott@db1:1521/ORCL` or `scott@db1:5432/myapp [pg]`.
    pub fn display_name(&self) -> String {
        let base = format!(
            "{}@{}:{}/{}",
            self.user,
            self.host,
            self.port,
            self.db_name()
        );
        match self.db_type {
            DbType::Oracle => base,
            DbType::Postgres => format!("{} [pg]", base),
        }
    }

    /// Oracle EZCONNECT string (`//host:port/service`). Only meaningful for Oracle.
    pub fn oracle_connect_string(&self) -> String {
        format!("//{}:{}/{}", self.host, self.port, self.service)
    }
}

impl Config {
    pub fn parse() -> Self {
        // Second-priority configuration source: a `.env` file in the working
        // directory, consulted as an explicit fallback map (never injected
        // into the process env, so real env vars keep their priority).
        // Precedence per setting: CLI flag > env var > `.env` > default.
        let dotenv_map = match dotenv::load_dotenv() {
            Some((info, map)) => {
                eprintln!("frog: {}", info.summary());
                map
            }
            None => std::collections::HashMap::new(),
        };

        let args = CliArgs::parse();
        let connection = Self::resolve_connection(&args, &dotenv_map);
        // Non-connection settings with no clap default conflict: dotenv fills
        // in only when neither CLI nor real env provided a value (the map
        // excludes keys already present in the real environment).
        let max_rows = dotenv_map
            .get("FROG_MAX_ROWS")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(args.max_rows);
        let autocommit = !args.no_autocommit && !dotenv_flag(&dotenv_map, "FROG_NO_AUTOCOMMIT");
        let extra_config = dotenv_map.get("FROG_CONFIG").map(PathBuf::from);
        let effective_config = args.config_file.clone().or(extra_config);
        // Tell the user when nothing configured the connection: this is the
        // "my env vars didn't reach the dialog" situation (e.g. `.env` in a
        // different directory, or unsupported variable names).
        if connection.user.is_empty()
            && is_default_target(&connection)
            && !cli_gave_connection()
            && !CONN_ENV_KEYS.iter().any(|k| std::env::var_os(k).is_some())
            && !CONN_ENV_KEYS.iter().any(|k| dotenv_map.contains_key(*k))
        {
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into());
            eprintln!(
                "frog: no connection settings from CLI flags, env vars or ./.env \
                 (looked for ./.env in '{}') — using {} defaults; \
                 see `frog --help` for the ORACLE_*/PG* variables",
                cwd, connection.db_type
            );
        }
        let extra_saved = Self::saved_entry(&connection);

        let config_file = load_config_file(effective_config.as_deref());

        let ui = config_file
            .as_ref()
            .and_then(|c| c.ui.clone())
            .unwrap_or_default();

        let max_history = config_file
            .as_ref()
            .and_then(|c| c.defaults.as_ref())
            .and_then(|d| d.max_history)
            .unwrap_or(1000);

        let mut saved_connections = config_file
            .as_ref()
            .and_then(|c| c.connections.clone())
            .unwrap_or_default();
        saved_connections.extend(extra_saved);

        Config {
            connection,
            ui,
            max_rows,
            autocommit,
            max_history,
            saved_connections,
        }
    }

    fn saved_entry(connection: &ConnectionParams) -> Vec<ConnectionEntry> {
        if connection.user.is_empty() || connection.host.is_empty() {
            return vec![];
        }
        vec![ConnectionEntry {
            // Do not retain the plaintext password in the saved list.
            name: connection.display_name(),
            host: connection.host.clone(),
            port: connection.port,
            service: connection.service.clone(),
            user: connection.user.clone(),
            password: None,
            db_type: Some(connection.db_type),
            database: Some(connection.database.clone()),
        }]
    }

    fn resolve_connection(
        args: &CliArgs,
        dotenv: &std::collections::HashMap<String, String>,
    ) -> ConnectionParams {
        // Tiers per setting: CLI-or-real-env (tier 1) > `.env` map (tier 2) >
        // connect-string value > built-in default. A connect string never
        // overrides tier-1 fields, so real `PG*`/`ORACLE_*` vars always beat a
        // `.env` `ORACLE_CONNECT`.
        let denv = |k: &str| dotenv.get(k).cloned().filter(|s| !s.is_empty());

        // Connect string source + its tier.
        let cs_tier1 = args.connect_string.clone().or_else(|| env_get("DATABASE_URL"));
        let cs_text = cs_tier1.clone().or_else(|| {
            denv("ORACLE_CONNECT").or_else(|| denv("DATABASE_URL"))
        });
        let cs_is_tier1 = cs_tier1.is_some();
        let parsed = cs_text.as_deref().map(parse_connect_string);
        let cs_db_type = parsed.as_ref().map(|p| p.db_type);

        // Tier 1: CLI flags (via clap) or real process env.
        let t1_host = args
            .host
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| env_get("PGHOST"));
        let t1_port = args.port.or_else(|| {
            env_get("PGPORT").and_then(|s| s.trim().parse::<u16>().ok())
        });
        let t1_service = args.service.clone().filter(|s| !s.is_empty());
        let t1_database = args
            .database
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| env_get("PGDATABASE"));
        let t1_user = args
            .user
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| env_get("PGUSER"));
        let t1_password = env_get("ORACLE_PASSWORD").or_else(|| env_get("PGPASSWORD"));
        let t1_db_type = match args.db_type.as_deref() {
            Some(t) => match t.parse::<DbType>() {
                Ok(db_type) => Some(db_type),
                Err(e) => {
                    eprintln!("frog: warning: {} — using oracle", e);
                    Some(DbType::Oracle)
                }
            },
            None => None,
        };

        // Tier 2: `.env` map (already excludes keys set in the real env).
        let t2_host = denv("ORACLE_HOST").or_else(|| denv("PGHOST"));
        let t2_port = denv("ORACLE_PORT")
            .or_else(|| denv("PGPORT"))
            .and_then(|s| s.trim().parse::<u16>().ok());
        let t2_service = denv("ORACLE_SERVICE");
        let t2_database = denv("PGDATABASE");
        let t2_user = denv("ORACLE_USER").or_else(|| denv("PGUSER"));
        let t2_password = denv("ORACLE_PASSWORD").or_else(|| denv("PGPASSWORD"));
        let t2_db_type = denv("FROG_DB_TYPE").and_then(|t| t.parse::<DbType>().ok());

        // Backend: tier 1 > connect string > tier 2 > auto-detect > oracle.
        let db_type = t1_db_type.or(cs_db_type).or(t2_db_type).unwrap_or_else(|| {
            if t1_database.is_some() {
                return DbType::Postgres;
            }
            let pg_set = ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER", "PGPASSWORD"]
                .iter()
                .any(|k| std::env::var_os(k).is_some() || dotenv.contains_key(*k));
            let oracle_set = ["ORACLE_HOST", "ORACLE_PORT", "ORACLE_SERVICE", "ORACLE_USER"]
                .iter()
                .any(|k| std::env::var_os(k).is_some() || dotenv.contains_key(*k));
            if pg_set && !oracle_set {
                DbType::Postgres
            } else {
                DbType::Oracle
            }
        });

        // A connect descriptor belongs to its backend: when the final backend
        // differs from the descriptor's, its host/port are dropped (tier-1
        // values always survive). Names (service/database) are kept, since
        // `-S` doubles as a dbname alias.
        let (cs_host, cs_port) = match parsed.as_ref() {
            Some(p) if p.db_type == db_type => (p.host.clone(), p.port),
            // A connect descriptor belongs to its own backend: on a backend
            // switch its endpoint is dropped (names are kept, see above).
            Some(_) => (None, None),
            None => (None, None),
        };
        let cs_service = parsed.as_ref().and_then(|p| p.service.clone());
        let cs_database = parsed.as_ref().and_then(|p| p.database.clone());
        let cs_user = parsed
            .as_ref()
            .map(|p| p.user.clone())
            .filter(|s| !s.is_empty());
        let cs_password = parsed.as_ref().and_then(|p| p.password.clone());

        let host = t1_host
            .or(cs_host)
            .or(t2_host)
            .unwrap_or_else(|| String::from("localhost"));
        let port = t1_port
            .or(cs_port)
            .or(t2_port)
            .unwrap_or_else(|| db_type.default_port());
        let mut service = t1_service
            .or(cs_service)
            .or(t2_service)
            .unwrap_or_default();
        let mut database = t1_database
            .or(cs_database)
            .or(t2_database)
            .unwrap_or_default();
        let user = t1_user.or(cs_user).or(t2_user).unwrap_or_default();
        // Password order: tier-1 connect string > tier-1 env > tier-2
        // connect string > tier-2 env.
        let password = (if cs_is_tier1 { cs_password.clone() } else { None })
            .or(t1_password)
            .or(if cs_is_tier1 { None } else { cs_password })
            .or(t2_password);

        match db_type {
            DbType::Oracle => {
                if service.is_empty() {
                    service = database.clone();
                }
                if service.is_empty() {
                    service = String::from("ORCL");
                }
                database = service.clone();
            }
            DbType::Postgres => {
                if database.is_empty() {
                    database = service.clone();
                }
                if database.is_empty() {
                    database = default_pg_database(&user);
                }
                service = database.clone();
            }
        }

        ConnectionParams {
            db_type,
            host,
            port,
            service,
            database,
            user,
            password,
        }
    }

    /// Oracle EZCONNECT string for the startup connection (postgres callers
    /// should use the full `ConnectionParams` instead).
    pub fn connect_string(&self) -> String {
        match self.connection.db_type {
            DbType::Oracle => self.connection.oracle_connect_string(),
            DbType::Postgres => format!(
                "host={} port={} dbname={} user={}",
                self.connection.host,
                self.connection.port,
                self.connection.database,
                self.connection.user,
            ),
        }
    }
}

/// Default postgres database: the user name, else `postgres` (psql convention).
fn default_pg_database(user: &str) -> String {
    if user.is_empty() {
        String::from("postgres")
    } else {
        user.to_string()
    }
}

/// Every env var frog reads for connection setup (used for the "nothing
/// configured me" startup hint).
const CONN_ENV_KEYS: &[&str] = &[
    "ORACLE_CONNECT",
    "ORACLE_HOST",
    "ORACLE_PORT",
    "ORACLE_SERVICE",
    "ORACLE_USER",
    "ORACLE_PASSWORD",
    "FROG_DB_TYPE",
    "PGHOST",
    "PGPORT",
    "PGDATABASE",
    "PGUSER",
    "PGPASSWORD",
    "DATABASE_URL",
];

/// Whether the connection is just built-in defaults (nothing was configured).
fn is_default_target(c: &ConnectionParams) -> bool {
    c.host == "localhost"
        && c.port == c.db_type.default_port()
        && ((c.db_type == DbType::Oracle && c.service == "ORCL")
            || (c.db_type == DbType::Postgres && c.database == "postgres"))
}

/// Whether any connection-related CLI flag was passed (exact or `--flag=value`).
fn cli_gave_connection() -> bool {
    const FLAGS: &[&str] = &[
        "-H", "--host", "-P", "--port", "-S", "--service", "-D", "--database", "-U",
        "--user", "-d", "--connect-string", "--db-type",
    ];
    std::env::args().skip(1).any(|a| {
        FLAGS
            .iter()
            .any(|f| a == *f || a.starts_with(&format!("{}=", f)))
    })
}

/// Real (process) env var, ignoring empty values. (The `.env` map is never
/// injected into the process env, so this is always the real environment.)
fn env_get(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Parse a dotenv truthy flag (`1`/`true`/`yes`/`on`).
fn dotenv_flag(map: &std::collections::HashMap<String, String>, key: &str) -> bool {
    map.get(key).map(|s| {
        matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }).unwrap_or(false)
}

/// A connect string broken into fields. `None` = key absent (resolve applies
/// tiers/defaults). The environment is never consulted here.
struct ParsedConnect {
    db_type: DbType,
    host: Option<String>,
    port: Option<u16>,
    service: Option<String>,
    database: Option<String>,
    user: String,
    password: Option<String>,
}

fn parse_postgres_url(cs: &str) -> Option<ParsedConnect> {
    let rest = cs
        .strip_prefix("postgres://")
        .or_else(|| cs.strip_prefix("postgresql://"))?;
    // Split off query params (?sslmode=...) — ignored in v1 (NoTls).
    let rest = rest.split('?').next().unwrap_or(rest);
    // [user[:password]@]host[:port][/dbname]
    let (auth, hostpart) = match rest.rsplit_once('@') {
        Some((a, h)) => (a, h),
        None => ("", rest),
    };
    let (user, password) = match auth.split_once(':') {
        Some((u, p)) => (u.to_string(), Some(p.to_string()).filter(|s| !s.is_empty())),
        None => (auth.to_string(), None),
    };
    let (hostport, database) = match hostpart.split_once('/') {
        Some((h, d)) => (h, d.to_string()),
        None => (hostpart, String::new()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            (!h.is_empty()).then(|| h.to_string()),
            p.parse::<u16>().ok(),
        ),
        None => ((!hostport.is_empty()).then(|| hostport.to_string()), None),
    };
    let database = (!database.is_empty()).then(|| database.clone());
    Some(ParsedConnect {
        db_type: DbType::Postgres,
        host,
        port,
        service: database.clone(),
        database,
        user,
        password,
    })
}

fn parse_connect_string(cs: &str) -> ParsedConnect {
    if let Some(params) = parse_postgres_url(cs.trim()) {
        return params;
    }

    let normalized = cs.trim().trim_end_matches(';');
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut service: Option<String> = None;
    let mut database: Option<String> = None;
    let mut user = String::new();
    let mut password: Option<String> = None;
    let mut db_type: Option<DbType> = None;

    for part in normalized.split(';') {
        // split_once keeps '=' inside values (e.g. base64 passwords).
        if let Some((key, value)) = part.trim().split_once('=') {
            match key.to_ascii_uppercase().as_str() {
                "HOST" => host = Some(value.trim().to_string()),
                "PORT" => port = value.trim().parse::<u16>().ok(),
                "SERVICE_NAME" | "SERVICE" => service = Some(value.trim().to_string()),
                "DATABASE" | "DBNAME" | "DB" => database = Some(value.trim().to_string()),
                "DB_TYPE" | "DBTYPE" | "TYPE" => {
                    if let Ok(t) = value.parse::<DbType>() {
                        db_type = Some(t);
                    }
                }
                "USER" => user = value.trim().to_string(),
                "PASSWORD" => password = Some(value.to_string()),
                _ => {}
            }
        }
    }

    // Infer the backend when not stated explicitly.
    let db_type = db_type.unwrap_or_else(|| {
        if database.is_some() && service.is_none() {
            DbType::Postgres
        } else {
            DbType::Oracle
        }
    });
    // `-S` and `DATABASE=` are aliases: fill the missing side.
    if service.is_none() {
        service = database.clone();
    }
    if database.is_none() {
        database = service.clone();
    }

    ParsedConnect {
        db_type,
        host,
        port,
        service,
        database,
        user,
        password,
    }
}

fn load_config_file(path: Option<&Path>) -> Option<ConfigFile> {
    let try_paths: Vec<PathBuf> = if let Some(p) = path {
        vec![p.to_path_buf()]
    } else {
        let mut paths = Vec::new();
        if let Some(d) = dirs::config_dir() {
            paths.push(d.join("frog/config.yml"));
            paths.push(d.join("frog/config.yaml"));
        }
        paths
    };

    // Surface config load/parse problems on stderr instead of silently using
    // defaults, so misconfiguration isn't invisible to the user.
    let mut found_missing = Vec::new();
    for p in &try_paths {
        if !p.exists() {
            found_missing.push(p.clone());
            continue;
        }
        match std::fs::read_to_string(p) {
            Ok(contents) => match serde_yaml::from_str::<ConfigFile>(&contents) {
                Ok(cfg) => return Some(cfg),
                Err(e) => {
                    eprintln!(
                        "frog: warning: could not parse config '{}': {}",
                        p.display(),
                        e
                    );
                    return None;
                }
            },
            Err(e) => {
                eprintln!(
                    "frog: warning: could not read config '{}': {}",
                    p.display(),
                    e
                );
                return None;
            }
        }
    }

    // An explicit -f path that doesn't exist is a hard error worth flagging.
    if let Some(p) = path {
        if found_missing.iter().any(|x| x == p) {
            eprintln!("frog: warning: config file not found: '{}'", p.display());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parse_connect_string_without_user() {
        // parse_from reads process env: exclude env-mutating tests.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let args = CliArgs::parse_from([
            "frog",
            "-d",
            "host=localhost;port=1521;service_name=ORCL;user=scott",
        ]);
        assert_eq!(
            args.connect_string.as_deref(),
            Some("host=localhost;port=1521;service_name=ORCL;user=scott")
        );
        assert_eq!(args.user, None);
    }

    #[test]
    fn test_parse_connect_string() {
        let params = parse_connect_string(
            "host=myhost;port=1522;service_name=mysvc;user=myuser;password=mypass",
        );
        assert_eq!(params.db_type, DbType::Oracle);
        assert_eq!(params.host.as_deref(), Some("myhost"));
        assert_eq!(params.port, Some(1522));
        assert_eq!(params.service.as_deref(), Some("mysvc"));
        assert_eq!(params.user, "myuser");
        assert_eq!(params.password.as_deref(), Some("mypass"));
    }

    #[test]
    fn test_parse_connect_string_value_with_equals_and_mixed_case() {
        let params =
            parse_connect_string("Host=db1;Password=ab==cd;SERVICE_NAME=orcl;User=u");
        assert_eq!(params.host.as_deref(), Some("db1"));
        assert_eq!(params.password.as_deref(), Some("ab==cd"));
        assert_eq!(params.service.as_deref(), Some("orcl"));
        assert_eq!(params.user, "u");
    }

    #[test]
    fn test_parse_postgres_url() {
        let params = parse_connect_string("postgres://scott@db1.local:5432/myapp");
        assert_eq!(params.db_type, DbType::Postgres);
        assert_eq!(params.host.as_deref(), Some("db1.local"));
        assert_eq!(params.port, Some(5432));
        assert_eq!(params.database.as_deref(), Some("myapp"));
        assert_eq!(params.user, "scott");
    }

    #[test]
    fn test_parse_postgres_url_with_password_defaults_port() {
        let params =
            parse_connect_string("postgresql://bob:s3cret@db2/mydb");
        assert_eq!(params.db_type, DbType::Postgres);
        assert_eq!(params.port, None);
        assert_eq!(params.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn test_parse_pg_keyval() {
        let params =
            parse_connect_string("HOST=db1;PORT=5433;DATABASE=myapp;USER=bob");
        assert_eq!(params.db_type, DbType::Postgres);
        assert_eq!(params.port, Some(5433));
        assert_eq!(params.database.as_deref(), Some("myapp"));
        // Alias: service mirrors database for postgres.
        assert_eq!(params.service.as_deref(), Some("myapp"));
    }

    #[test]
    fn test_parse_keeps_absence() {
        // Absent keys stay absent so resolve can apply tiers/defaults.
        let params = parse_connect_string("USER=bob");
        assert_eq!(params.db_type, DbType::Oracle);
        assert_eq!(params.host, None);
        assert_eq!(params.port, None);
        assert_eq!(params.service, None);
        assert_eq!(params.database, None);
    }

    /// Save/remove process env keys for the test, restoring them on drop.
    /// Tests share one process, so env-mutating tests serialize on ENV_LOCK.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn clear(keys: &[&str]) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = keys
                .iter()
                .map(|k| (k.to_string(), std::env::var(k).ok()))
                .collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { saved, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// Parse args as the real binary would (clap picks up `env = ...`
    /// values), so env handling is tested end to end. Callers must hold
    /// the ENV_LOCK via EnvGuard.
    fn parse_args() -> CliArgs {
        CliArgs::parse_from(["frog"])
    }

    #[test]
    fn resolve_honors_pg_env() {
        let _g = EnvGuard::clear(CONN_ENV_KEYS);
        std::env::set_var("PGHOST", "pg.test");
        std::env::set_var("PGDATABASE", "mydb");
        std::env::set_var("PGUSER", "bob");
        let empty = std::collections::HashMap::new();
        let c = Config::resolve_connection(&parse_args(), &empty);
        assert_eq!(c.db_type, DbType::Postgres);
        assert_eq!(c.host, "pg.test");
        assert_eq!(c.port, 5432);
        assert_eq!(c.database, "mydb");
        assert_eq!(c.service, "mydb");
        assert_eq!(c.user, "bob");
    }

    #[test]
    fn resolve_honors_oracle_env() {
        let _g = EnvGuard::clear(CONN_ENV_KEYS);
        std::env::set_var("ORACLE_HOST", "ora.test");
        std::env::set_var("ORACLE_SERVICE", "XE");
        std::env::set_var("ORACLE_USER", "scott");
        let empty = std::collections::HashMap::new();
        let c = Config::resolve_connection(&parse_args(), &empty);
        assert_eq!(c.db_type, DbType::Oracle);
        assert_eq!(c.host, "ora.test");
        assert_eq!(c.port, 1521);
        assert_eq!(c.service, "XE");
        assert_eq!(c.user, "scott");
    }

    #[test]
    fn resolve_honors_database_url() {
        let _g = EnvGuard::clear(CONN_ENV_KEYS);
        std::env::set_var("DATABASE_URL", "postgres://bob@db:5433/app");
        let empty = std::collections::HashMap::new();
        let c = Config::resolve_connection(&parse_args(), &empty);
        assert_eq!(c.db_type, DbType::Postgres);
        assert_eq!(c.host, "db");
        assert_eq!(c.port, 5433);
        assert_eq!(c.database, "app");
        assert_eq!(c.user, "bob");
    }

    #[test]
    fn resolve_real_env_beats_dotenv_connect_string() {
        // The reported bug: a `.env` ORACLE_CONNECT must not override real
        // PG* variables — per setting, env wins over `.env`.
        let _g = EnvGuard::clear(CONN_ENV_KEYS);
        std::env::set_var("FROG_DB_TYPE", "postgres");
        std::env::set_var("PGHOST", "pg.real");
        std::env::set_var("PGDATABASE", "realdb");
        std::env::set_var("PGUSER", "realuser");
        std::env::set_var("PGPASSWORD", "realpw");
        let dotenv: std::collections::HashMap<String, String> = [(
            "ORACLE_CONNECT".to_string(),
            "HOST=ora.env;PORT=1521;SERVICE_NAME=ORCL;USER=envuser;PASSWORD=envpw".to_string(),
        )]
        .into_iter()
        .collect();
        let c = Config::resolve_connection(&parse_args(), &dotenv);
        assert_eq!(c.db_type, DbType::Postgres);
        assert_eq!(c.host, "pg.real");
        // Endpoint came from the Oracle descriptor for a Postgres target:
        // dropped in favor of the backend default.
        assert_eq!(c.port, 5432);
        assert_eq!(c.database, "realdb");
        assert_eq!(c.user, "realuser");
        assert_eq!(c.password.as_deref(), Some("realpw"));
    }

    #[test]
    fn resolve_dotenv_applies_when_env_empty() {
        // `.env` alone still configures everything.
        let _g = EnvGuard::clear(CONN_ENV_KEYS);
        let dotenv: std::collections::HashMap<String, String> = [
            ("ORACLE_HOST".to_string(), "ora.env".to_string()),
            ("ORACLE_SERVICE".to_string(), "XE".to_string()),
            ("ORACLE_USER".to_string(), "envuser".to_string()),
        ]
        .into_iter()
        .collect();
        let c = Config::resolve_connection(&parse_args(), &dotenv);
        assert_eq!(c.db_type, DbType::Oracle);
        assert_eq!(c.host, "ora.env");
        assert_eq!(c.port, 1521);
        assert_eq!(c.service, "XE");
        assert_eq!(c.user, "envuser");
    }
}
