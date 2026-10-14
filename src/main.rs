use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use futures::StreamExt;
use ratatui::crossterm::event::EventStream;

use kdvc::api::Client;
use kdvc::app::{App, Overlay};
use kdvc::db::Db;
use kdvc::{config, ui};

/// Visualiser et nettoyer les anciennes versions des fichiers d'un kDrive Infomaniak.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Drive à ouvrir (identifiant ou nom).
    #[arg(long)]
    drive: Option<String>,
    /// Forcer un nouveau scan au démarrage.
    #[arg(long)]
    rescan: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let token = config::load_token()?;
    // KDVC_API_URL permet de pointer vers un serveur de test.
    let client = match std::env::var("KDVC_API_URL") {
        Ok(url) => Client::with_options(token, url, kdvc::api::DEFAULT_MIN_INTERVAL)?,
        Err(_) => Client::new(token)?,
    };
    let db_path = config::cache_db_path()?;
    let db = Db::open(&db_path)?;

    eprintln!("Connexion à l'API Infomaniak…");
    let drives = client.list_drives().await?;
    if drives.is_empty() {
        bail!("aucun kDrive accessible avec ce token");
    }
    db.upsert_drives(&drives)?;

    let preselected = match &args.drive {
        Some(wanted) => {
            let found = drives
                .iter()
                .find(|d| d.id.to_string() == *wanted || d.name.eq_ignore_ascii_case(wanted));
            match found {
                Some(d) => Some(d.id),
                None => bail!(
                    "drive « {wanted} » introuvable. Drives disponibles : {}",
                    drives
                        .iter()
                        .map(|d| format!("{} (#{})", d.name, d.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        None if drives.len() == 1 => Some(drives[0].id),
        None => None,
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(client, db, db_path, tx, drives);
    match preselected {
        Some(id) => app.select_drive(id, args.rescan)?,
        None => {
            app.rescan_on_select = args.rescan;
            app.open_drive_picker();
        }
    }

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app, rx).await;
    ratatui::restore();
    res
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<kdvc::events::AppEvent>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => app.on_terminal_event(ev)?,
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(msg) = rx.recv() => app.on_app_event(msg)?,
            _ = tick.tick() => {}
        }
        // Après une sélection de drive à l'aveugle, le picker peut se refermer sans drive.
        if app.drive.is_none() && matches!(app.overlay, Overlay::None) {
            app.open_drive_picker();
        }
    }
    Ok(())
}
