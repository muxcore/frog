pub mod dotenv;

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Parser, Clone, Debug)]
#[command(
    name = "frog",
    about = "Terminal Oracle client",
    after_help = "\nExamples:\n\n\
Connect with a full connection string (host/port/service/user/password):\n\
    frog -d \"HOST=db1.local;PORT=1521;SERVICE_NAME=ORCL;USER=scott;PASSWORD=tiger\"\n\n\
Connect with individual parameters (env variables can be used instead):\n\
    frog -H db1.local -P 1521 -S ORCL -U scott\n\n\
Same connection using only environment variables:\n\
    ORACLE_HOST=db1.local ORACLE_SERVICE=ORCL ORACLE_USER=scott ORACLE_PASSWORD=tiger frog\n"
)]
pub struct CliArgs {
    #[arg(short = 'H', long, env = "ORACLE_HOST", default_value = "localhost")]
    pub host: String,

    #[arg(short = 'P', long, env = "ORACLE_PORT", default_value = "1521")]
    pub port: u16,

    #[arg(short = 'S', long, env = "ORACLE_SERVICE", default_value = "ORCL")]
    pub service: String,

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: Option<String>,
    pub tab_size: Option<usize>,
    pub date_format: Option<String>,
    pub null_display: Option<String>,
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
    pub host: String,
    pub port: u16,
    pub service: String,
    pub user: String,
    pub password: Option<String>,
}

impl Config {
    pub fn parse() -> Self {
        // Second-priority configuration source: a `.env` file in the working
        // directory. Keys already set in the real environment are not
        // overridden, so precedence is CLI > env var > .env > default.
        if let Some(info) = dotenv::apply_dotenv() {
            eprintln!("frog: {}", info.summary());
        }

        let args = CliArgs::parse();

        let (mut connection, mut extra_saved) = if let Some(ref cs) = args.connect_string {
            parse_connect_string(cs)
        } else {
            (
                ConnectionParams {
                    host: args.host,
                    port: args.port,
                    service: args.service,
                    user: args.user.clone().unwrap_or_default(),
                    // Password comes from ORACLE_PASSWORD env (never a CLI flag),
                    // or is prompted interactively at startup in main().
                    password: std::env::var("ORACLE_PASSWORD")
                        .ok()
                        .filter(|p| !p.is_empty()),
                },
                vec![],
            )
        };

        if let Some(ref u) = args.user {
            if !u.is_empty() {
                connection.user = u.clone();
                if extra_saved.is_empty() && !connection.host.is_empty() {
                    extra_saved = vec![ConnectionEntry {
                        name: format!(
                            "{}@{}:{}/{}",
                            connection.user, connection.host, connection.port, connection.service
                        ),
                        host: connection.host.clone(),
                        port: connection.port,
                        service: connection.service.clone(),
                        user: connection.user.clone(),
                        // Do not retain the plaintext password in the saved list.
                        password: None,
                    }];
                } else if !extra_saved.is_empty() {
                    extra_saved[0].user = connection.user.clone();
                    extra_saved[0].name = format!(
                        "{}@{}:{}/{}",
                        connection.user,
                        extra_saved[0].host,
                        extra_saved[0].port,
                        extra_saved[0].service
                    );
                }
            }
        }

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

    pub fn connect_string(&self) -> String {
        format!(
            "//{}:{}/{}",
            self.connection.host, self.connection.port, self.connection.service
        )
    }
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

fn parse_connect_string(cs: &str) -> (ConnectionParams, Vec<ConnectionEntry>) {
    let normalized = cs.trim().trim_end_matches(';');
    let mut host = String::from("localhost");
    let mut port = 1521u16;
    let mut service = String::from("ORCL");
    let mut user = String::new();
    let mut password: Option<String> = None;

    for part in normalized.split(';') {
        // split_once keeps '=' inside values (e.g. base64 passwords).
        if let Some((key, value)) = part.trim().split_once('=') {
            match key.to_ascii_uppercase().as_str() {
                "HOST" => host = value.trim().to_string(),
                "PORT" => port = value.trim().parse().unwrap_or(1521),
                "SERVICE_NAME" => service = value.trim().to_string(),
                "USER" => user = value.trim().to_string(),
                "PASSWORD" => password = Some(value.to_string()),
                _ => {}
            }
        }
    }

    let params = ConnectionParams {
        host: host.clone(),
        port,
        service: service.clone(),
        user: user.clone(),
        password: password.clone(),
    };

    let saved = if !user.is_empty() {
        vec![ConnectionEntry {
            name: format!("{}@{}:{}/{}", user, host, port, service),
            host,
            user,
            service,
            port,
            password: None,
        }]
    } else {
        vec![]
    };

    (params, saved)
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
}
