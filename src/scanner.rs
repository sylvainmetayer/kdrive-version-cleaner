use std::path::PathBuf;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{Client, Depth, ROOT_DIR_ID, RemoteFile};
use crate::db::Db;
use crate::events::AppEvent;

/// Scanne toute l'arborescence d'un drive et remplace le cache.
///
/// L'API refuse `depth=unlimited` sur la racine : on liste d'abord la racine,
/// puis chaque dossier de premier niveau en profondeur illimitée.
pub async fn scan(client: Client, db_path: PathBuf, drive_id: i64, tx: UnboundedSender<AppEvent>) {
    let result = run(&client, db_path, drive_id, &tx).await;
    let _ = tx.send(AppEvent::ScanDone(result.map_err(|e| format!("{e:#}"))));
}

async fn list_all(
    client: &Client,
    drive_id: i64,
    dir_id: i64,
    depth: Depth,
    files: &mut Vec<RemoteFile>,
    mut on_page: impl FnMut(usize),
) -> Result<()> {
    let mut cursor: Option<String> = None;
    loop {
        let page = client
            .list_files(drive_id, dir_id, depth, cursor.as_deref())
            .await?;
        files.extend(page.items.into_iter().filter(|f| !f.is_trashed()));
        on_page(files.len());
        if !page.has_more {
            return Ok(());
        }
        cursor = page.cursor;
    }
}

async fn run(
    client: &Client,
    db_path: PathBuf,
    drive_id: i64,
    tx: &UnboundedSender<AppEvent>,
) -> Result<()> {
    let progress = |files: usize, done: usize, total: usize, current: &str| {
        let _ = tx.send(AppEvent::ScanProgress {
            files,
            dirs_done: done,
            dirs_total: total,
            current: current.to_string(),
        });
    };

    let mut files = Vec::new();
    progress(0, 0, 0, "/");
    list_all(
        client,
        drive_id,
        ROOT_DIR_ID,
        Depth::Child,
        &mut files,
        |n| progress(n, 0, 0, "/"),
    )
    .await?;

    let top_dirs: Vec<(i64, String)> = files
        .iter()
        .filter(|f| f.is_dir())
        .map(|f| (f.id, f.name.clone()))
        .collect();
    let total = top_dirs.len();
    for (i, (id, name)) in top_dirs.iter().enumerate() {
        progress(files.len(), i, total, name);
        list_all(client, drive_id, *id, Depth::Unlimited, &mut files, |n| {
            progress(n, i, total, name)
        })
        .await?;
    }
    progress(files.len(), total, total, "enregistrement…");

    // Une même entrée peut apparaître deux fois si l'API renvoie des doublons entre pages.
    files.sort_by_key(|f| f.id);
    files.dedup_by_key(|f| f.id);

    let now = crate::cleaner::now();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut db = Db::open(&db_path)?;
        db.replace_tree(drive_id, &files, now)
    })
    .await??;
    Ok(())
}
