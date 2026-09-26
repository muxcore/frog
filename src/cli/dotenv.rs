use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Keys frog reads from the environment; these are also picked up from a
/// `.env` file in the current working directory.
const ENV_KEYS: &[&str] = &[
    "ORACLE_CONNECT",
    "ORACLE_HOST",
    "ORACLE_PORT",
    "ORACLE_SERVICE",
    "ORACLE_USER",
    "ORACLE_PASSWORD",
    "FROG_CONFIG",
    "FROG_MAX_ROWS",
    "FROG_NO_AUTOCOMMIT",
    "FROG_DB_TYPE",
    "PGHOST",
    "PGPORT",
    "PGDATABASE",
    "PGUSER",
    "PGPASSWORD",
    "DATABASE_URL",
];

/// What was read from a `.env` file.
pub struct DotEnvInfo {
    pub path: PathBuf,
    /// Keys whose values were applied (not present in the real environment).
    pub applied: Vec<String>,
    /// Keys present in `.env` but ignored because the real environment wins.
    pub overridden: Vec<String>,
}

/// Parse `.env`-style contents: `KEY=VALUE` lines, `#` comments, optional
/// `export ` prefix and matching single/double quotes around values.
fn parse_dotenv(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        out.push((key.to_string(), value.to_string()));
    }
    out
}

/// Load `<cwd>/.env` (if any) WITHOUT touching the process environment.
///
/// Returns the summary info plus a map of the values to use as fallbacks:
/// only keys absent from the real environment are included, so precedence
/// stays CLI flag > env var > `.env` > built-in default *per setting*.
/// (An earlier design exported `.env` via `set_var`, which made `.env`
/// values indistinguishable from real env vars and let a `.env`
/// connect string silently beat real `PG*`/`ORACLE_*` variables.)
pub fn load_dotenv() -> Option<(DotEnvInfo, HashMap<String, String>)> {
    let path = PathBuf::from(".env");
    load_from(&path)
}

fn load_from(path: &Path) -> Option<(DotEnvInfo, HashMap<String, String>)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut info = DotEnvInfo {
        path: path.to_path_buf(),
        applied: Vec::new(),
        overridden: Vec::new(),
    };
    let mut map = HashMap::new();
    for (key, value) in parse_dotenv(&contents) {
        if !ENV_KEYS.contains(&key.as_str()) {
            continue;
        }
        if std::env::var_os(&key).is_some() {
            info.overridden.push(key);
            continue;
        }
        info.applied.push(key.clone());
        map.insert(key, value);
    }
    Some((info, map))
}

/// Human-readable one-liner about what the `.env` file contributed.
impl DotEnvInfo {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.applied.is_empty() {
            parts.push(format!("using {}", self.applied.join(", ")));
        }
        if !self.overridden.is_empty() {
            parts.push(format!(
                "ignored {} (set in environment)",
                self.overridden.join(", ")
            ));
        }
        if parts.is_empty() {
            format!("found '{}' but no relevant keys", self.path.display())
        } else {
            format!("'{}': {}", self.path.display(), parts.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_entries_and_comments() {
        let vars = parse_dotenv(
            "# comment\nORACLE_HOST=db1\n\nexport ORACLE_PORT=1522\nORACLE_USER='scott'\n\
             ORACLE_PASSWORD=\"ti=ger\"\nNOT_A_FROG_KEY=x\nbroken line\n",
        );
        assert_eq!(
            vars,
            vec![
                ("ORACLE_HOST".into(), "db1".into()),
                ("ORACLE_PORT".into(), "1522".into()),
                ("ORACLE_USER".into(), "scott".into()),
                ("ORACLE_PASSWORD".into(), "ti=ger".into()),
                ("NOT_A_FROG_KEY".into(), "x".into()),
            ]
        );
    }

    #[test]
    fn summary_reports_applied_and_overridden() {
        let info = DotEnvInfo {
            path: PathBuf::from("./.env"),
            applied: vec!["ORACLE_HOST".into()],
            overridden: vec!["ORACLE_USER".into()],
        };
        assert_eq!(
            info.summary(),
            "'./.env': using ORACLE_HOST; ignored ORACLE_USER (set in environment)"
        );
    }
}
