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
    FROG_DB_TYPE=postgres PGHOST=db1.local PGDATABASE=myapp PGUSER=scott frog\n"
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
        // directory. Keys already set in the real environment are not
        // overridden, so precedence is CLI > env var > `.env` > default.
        if let Some(info) = dotenv::apply_dotenv() {
            eprintln!("frog: {}", info.summary());
        }

        let args = CliArgs::parse();
        let connection = Self::resolve_connection(&args);
        let extra_saved = Self::saved_entry(&connection);

        let config_file = load_config_file(args.config_file.as_deref());

        let ui = config_file
            .as_ref()
            .and_then(|c| c.ui.clone())
            .unwrap_or_default();

        let max_rows = args.max_rows;
        let autocommit = !args.no_autocommit;
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

    fn resolve_connection(args: &CliArgs) -> ConnectionParams {
        // Full connect string wins for host/port/service/database; explicit
        // user/db-type/database flags still override its components (legacy:
        // `-U` overrode the connect-string user).
        if let Some(ref cs) = args.connect_string {
            let (mut params, _) = parse_connect_string(cs);
            if let Some(ref u) = args.user {
                if !u.is_empty() {
                    params.user = u.clone();
                }
            }
            if let Some(ref t) = args.db_type {
                if let Ok(db_type) = t.parse::<DbType>() {
                    params.db_type = db_type;
                    if params.port == 0 {
                        params.port = db_type.default_port();
                    }
                }
            }
            if let Some(ref d) = args.database {
                params.database = d.clone();
                if params.service.is_empty() {
                    params.service = d.clone();
                }
            }
            if let Some(ref s) = args.service {
                params.service = s.clone();
                if params.database.is_empty() {
                    params.database = s.clone();
                }
            }
            if let Some(ref h) = args.host {
                params.host = h.clone();
            }
            if let Some(p) = args.port {
                params.port = p;
            }
            // Fill postgres database default from user when still empty.
            if params.db_type == DbType::Postgres && params.database.is_empty() {
                params.database = default_pg_database(&params.user);
                if params.service.is_empty() {
                    params.service = params.database.clone();
                }
            }
            return params;
        }

        let db_type = resolve_db_type(args);
        let host = args
            .host
            .clone()
            .or_else(|| std::env::var("PGHOST").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| String::from("localhost"));
        let port = args.port.or_else(|| {
            std::env::var("PGPORT")
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok())
        }).unwrap_or_else(|| db_type.default_port());
        let service_opt = args.service.clone();
        let database_opt = args
            .database
            .clone()
            .or_else(|| std::env::var("PGDATABASE").ok().filter(|s| !s.is_empty()));
        let user = args
            .user
            .clone()
            .or_else(|| std::env::var("PGUSER").ok().filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let password = std::env::var("ORACLE_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty())
            .or_else(|| std::env::var("PGPASSWORD").ok().filter(|p| !p.is_empty()));

        let (service, database) = match db_type {
            DbType::Oracle => {
                let service = service_opt
                    .or(database_opt)
                    .unwrap_or_else(|| String::from("ORCL"));
                let database = service.clone();
                (service, database)
            }
            DbType::Postgres => {
                let database = database_opt.or(service_opt).unwrap_or_else(|| default_pg_database(&user));
                let service = database.clone();
                (service, database)
            }
        };

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

/// Resolve the backend: explicit `--db-type` wins; otherwise postgres is
/// selected when PG* variables are present and no ORACLE_* ones are.
fn resolve_db_type(args: &CliArgs) -> DbType {
    if let Some(ref t) = args.db_type {
        match t.parse::<DbType>() {
            Ok(db_type) => return db_type,
            Err(e) => {
                eprintln!("frog: warning: {} — using oracle", e);
                return DbType::Oracle;
            }
        }
    }
    if args.database.is_some() {
        return DbType::Postgres;
    }
    let pg_set = ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER", "PGPASSWORD"]
        .iter()
        .any(|k| std::env::var_os(k).is_some());
    let oracle_set = ["ORACLE_HOST", "ORACLE_PORT", "ORACLE_SERVICE", "ORACLE_USER"]
        .iter()
        .any(|k| std::env::var_os(k).is_some());
    if pg_set && !oracle_set {
        DbType::Postgres
    } else {
        DbType::Oracle
    }
}

fn parse_postgres_url(cs: &str) -> Option<ConnectionParams> {
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
            h.to_string(),
            p.parse::<u16>().unwrap_or(DbType::Postgres.default_port()),
        ),
        None => (hostport.to_string(), DbType::Postgres.default_port()),
    };
    let host = if host.is_empty() { "localhost".into() } else { host };
    let database = if database.is_empty() {
        default_pg_database(&user)
    } else {
        database
    };
    Some(ConnectionParams {
        db_type: DbType::Postgres,
        host,
        port,
        service: database.clone(),
        database,
        user,
        password,
    })
}

fn parse_connect_string(cs: &str) -> (ConnectionParams, Vec<ConnectionEntry>) {
    if let Some(params) = parse_postgres_url(cs.trim()) {
        let saved = if !params.user.is_empty() {
            vec![ConnectionEntry {
                name: params.display_name(),
                host: params.host.clone(),
                port: params.port,
                service: params.service.clone(),
                user: params.user.clone(),
                password: None,
                db_type: Some(DbType::Postgres),
                database: Some(params.database.clone()),
            }]
        } else {
            vec![]
        };
        return (
            ConnectionParams {
                password: password_from_env_or(params.password),
                ..params
            },
            saved,
        );
    }

    let normalized = cs.trim().trim_end_matches(';');
    let mut host = String::from("localhost");
    let mut port = 0u16;
    let mut service = String::new();
    let mut database = String::new();
    let mut user = String::new();
    let mut password: Option<String> = None;
    let mut db_type: Option<DbType> = None;

    for part in normalized.split(';') {
        // split_once keeps '=' inside values (e.g. base64 passwords).
        if let Some((key, value)) = part.trim().split_once('=') {
            match key.to_ascii_uppercase().as_str() {
                "HOST" => host = value.trim().to_string(),
                "PORT" => port = value.trim().parse().unwrap_or(0),
                "SERVICE_NAME" | "SERVICE" => service = value.trim().to_string(),
                "DATABASE" | "DBNAME" | "DB" => database = value.trim().to_string(),
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
    let db_type = db_type.unwrap_or(if !database.is_empty() && service.is_empty() {
        DbType::Postgres
    } else {
        DbType::Oracle
    });
    if port == 0 {
        port = db_type.default_port();
    }
    // `-S` and `DATABASE=` are aliases: fill the missing side.
    if service.is_empty() && !database.is_empty() {
        service = database.clone();
    }
    if database.is_empty() {
        database = if db_type == DbType::Postgres {
            if service.is_empty() {
                default_pg_database(&user)
            } else {
                service.clone()
            }
        } else {
            if service.is_empty() {
                service = String::from("ORCL");
            }
            service.clone()
        };
    }
    if db_type == DbType::Oracle && service.is_empty() {
        service = String::from("ORCL");
    }

    let params = ConnectionParams {
        db_type,
        host: host.clone(),
        port,
        service: service.clone(),
        database: database.clone(),
        user: user.clone(),
        password: password_from_env_or(password),
    };

    let saved = if !user.is_empty() {
        vec![ConnectionEntry {
            name: params.display_name(),
            host,
            user,
            service,
            port,
            password: None,
            db_type: Some(db_type),
            database: Some(database),
        }]
    } else {
        vec![]
    };

    (params, saved)
}

/// Password precedence: connect-string component wins, else ORACLE_/PGPASSWORD env.
fn password_from_env_or(connect_password: Option<String>) -> Option<String> {
    if connect_password.is_some() {
        return connect_password;
    }
    std::env::var("ORACLE_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::var("PGPASSWORD").ok().filter(|p| !p.is_empty()))
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
        let (params, _saved) = parse_connect_string(
            "host=myhost;port=1522;service_name=mysvc;user=myuser;password=mypass",
        );
        assert_eq!(params.db_type, DbType::Oracle);
        assert_eq!(params.host, "myhost");
        assert_eq!(params.port, 1522);
        assert_eq!(params.service, "mysvc");
        assert_eq!(params.user, "myuser");
        assert_eq!(params.password.as_deref(), Some("mypass"));
    }

    #[test]
    fn test_parse_connect_string_value_with_equals_and_mixed_case() {
        let (params, _saved) =
            parse_connect_string("Host=db1;Password=ab==cd;SERVICE_NAME=orcl;User=u");
        assert_eq!(params.host, "db1");
        assert_eq!(params.password.as_deref(), Some("ab==cd"));
        assert_eq!(params.service, "orcl");
        assert_eq!(params.user, "u");
    }

    #[test]
    fn test_parse_postgres_url() {
        let (params, _saved) = parse_connect_string("postgres://scott@db1.local:5432/myapp");
        assert_eq!(params.db_type, DbType::Postgres);
        assert_eq!(params.host, "db1.local");
        assert_eq!(params.port, 5432);
        assert_eq!(params.database, "myapp");
        assert_eq!(params.user, "scott");
    }

    #[test]
    fn test_parse_postgres_url_with_password_defaults_port() {
        let (params, _saved) =
            parse_connect_string("postgresql://bob:s3cret@db2/mydb");
        assert_eq!(params.db_type, DbType::Postgres);
        assert_eq!(params.port, 5432);
        assert_eq!(params.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn test_parse_pg_keyval() {
        let (params, _saved) =
            parse_connect_string("HOST=db1;PORT=5433;DATABASE=myapp;USER=bob");
        assert_eq!(params.db_type, DbType::Postgres);
        assert_eq!(params.port, 5433);
        assert_eq!(params.database, "myapp");
        // Alias: service mirrors database for postgres.
        assert_eq!(params.service, "myapp");
    }
}
