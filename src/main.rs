pub mod cli;
pub mod db;
pub mod tui;

use cli::Config;
use db::SessionManager;
use tui::App;

fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let session_manager = SessionManager::new();

    let mut app = App::new(config, session_manager)?;
    app.run()?;

    Ok(())
}
