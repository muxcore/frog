use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Parser, Clone, Debug)]
#[command(name = "frog", about = "Terminal Oracle client")]
pub struct CliArgs {
    #[arg(short = 'H', long, env = "ORACLE_HOST", default_value = "localhost")]
    pub host: String,

    #[arg(short = 'P', long, env = "ORACLE_PORT", default_value = "1521")]
    pub port: u16,

    #[arg(short = 'S', long, env = "ORACLE_SERVICE", default_value = "ORCL")]
    pub service: String,

    #[arg(short = 'U', long = "user", env = "ORACLE_USER")]
    pub user: Option<String>,

    #[arg(short = 'W', long = "password", env = "ORACLE_PASSWORD")]
    pub password: Option<String>,

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
                    password: args.password,
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
                        password: connection.password.clone(),
                    }];
                } else if !extra_saved.is_empty() {
                    extra_saved[0].user = connection.user.clone();
                    extra_saved[0].name = format!(
                        "{}@{}:{}/{}",
                        connection.user, extra_saved[0].host, extra_saved[0].port, extra_saved[0].service
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

    pub fn connect_string(&self, password: Option<&str>) -> String {
        let _pw = password
            .or(self.connection.password.as_deref())
            .unwrap_or("");
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
        let part = part.trim();
        if part.starts_with("HOST=") || part.starts_with("host=") {
            host = part.split('=').nth(1).unwrap_or("localhost").to_string();
        } else if part.starts_with("PORT=") || part.starts_with("port=") {
            port = part
                .split('=')
                .nth(1)
                .and_then(|p| p.parse().ok())
                .unwrap_or(1521);
        } else if part.starts_with("SERVICE_NAME=") || part.starts_with("service_name=") {
            service = part.split('=').nth(1).unwrap_or("ORCL").to_string();
        } else if part.starts_with("USER=") || part.starts_with("user=") {
            user = part.split('=').nth(1).unwrap_or("").to_string();
        } else if part.starts_with("PASSWORD=") || part.starts_with("password=") {
            password = Some(part.split('=').nth(1).unwrap_or("").to_string());
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
            password,
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

    for p in try_paths {
        if p.exists() {
            if let Ok(contents) = std::fs::read_to_string(&p) {
                if let Ok(cfg) = serde_yaml::from_str::<ConfigFile>(&contents) {
                    return Some(cfg);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parse_connect_string_without_user() {
        let args = CliArgs::parse_from(["frog", "-d", "host=localhost;port=1521;service_name=ORCL;user=scott"]);
        assert_eq!(args.connect_string.as_deref(), Some("host=localhost;port=1521;service_name=ORCL;user=scott"));
        assert_eq!(args.user, None);
    }

    #[test]
    fn test_parse_connect_string() {
        let (params, _saved) = parse_connect_string("host=myhost;port=1522;service_name=mysvc;user=myuser;password=mypass");
        assert_eq!(params.host, "myhost");
        assert_eq!(params.port, 1522);
        assert_eq!(params.service, "mysvc");
        assert_eq!(params.user, "myuser");
        assert_eq!(params.password.as_deref(), Some("mypass"));
    }
}