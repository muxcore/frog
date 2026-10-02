pub mod cli;
pub mod db;
pub mod tui;

use cli::Config;
use db::SessionManager;
use tui::App;

fn main() -> anyhow::Result<()> {
    let mut config = Config::parse();

    // Prompt interactively (echo-off) for a missing password. We never accept a
    // password on the command line, so it can't leak via `ps`/`/proc`.
    if !config.connection.user.is_empty() && config.connection.password.is_none() {
        use std::io::Write;
        eprint!(
            "Enter password for {} at {}: ",
            config.connection.user, config.connection.host
        );
        std::io::stderr().flush()?;
        let pw = rpassword::prompt_password("")?;
        config.connection.password = Some(pw);
    }

    let mut session_manager = SessionManager::new();
    session_manager.max_history = config.max_history;
    session_manager.autocommit = config.autocommit;
    session_manager.max_rows = config.max_rows;
    session_manager.session_refresh_secs = config.session_refresh_secs;
    session_manager.null_display = config
        .ui
        .null_display
        .clone()
        .unwrap_or_else(|| "(NULL)".into());

    let mut app = App::new(config, session_manager)?;
    app.run()?;

    Ok(())
}
