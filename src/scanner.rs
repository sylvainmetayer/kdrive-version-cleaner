use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use futures::{StreamExt, stream};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{Client, Depth, ROOT_DIR_ID, RemoteFile};
use crate::db::{Db, FailedDir};
use crate::events::AppEvent;

/// Scanne toute l'arborescence d'un drive et remplace le cache.
///
/// L'API refuse `depth=unlimited` sur la racine : on liste d'abord la racine,
/// puis les dossiers de premier niveau en profondeur illimitée, plusieurs à la fois.
/// Un dossier en échec est signalé et garde son contenu du scan précédent ;
/// si tous échouent, le cache n'est pas modifié.
pub async fn scan(client: Client, db_path: PathBuf, drive_id: i64, tx: UnboundedSender<AppEvent>) {
    let result = run(&client, db_path, drive_id, &tx).await;
    let _ = tx.send(AppEvent::ScanDone(result.map_err(|e| format!("{e:#}"))));
}

/// Dossiers de premier niveau listés en même temps. Une page `depth=unlimited`
/// peut prendre plusieurs secondes côté serveur : en paralléliser quelques-unes
/// rapproche du plafond de 60 requêtes/min, que le rate limiter partagé garantit.
pub const SCAN_CONCURRENCY: usize = 4;

/// Liste un dossier page par page ; `on_page` reçoit le nombre d'éléments ajoutés.
async fn list_all(
    client: &Client,
    drive_id: i64,
    dir_id: i64,
    depth: Depth,
    files: &mut Vec<RemoteFile>,
    on_page: impl Fn(usize),
) -> Result<()> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    loop {
        let page = client
            .list_files(drive_id, dir_id, depth, cursor.as_deref())
            .await?;
        let before = files.len();
        files.extend(page.items.into_iter().filter(|f| !f.is_trashed()));
        on_page(files.len() - before);
        if !page.has_more {
            return Ok(());
        }
        // Un curseur déjà vu ferait boucler le scan indéfiniment.
        if let Some(c) = &page.cursor
            && !seen_cursors.insert(c.clone())
        {
            anyhow::bail!("dossier {dir_id} : curseur répété, pagination bloquée");
        }
        cursor = page.cursor;
    }
}

/// Compteurs partagés entre les dossiers listés en parallèle.
struct Progress<'a> {
    tx: &'a UnboundedSender<AppEvent>,
    files: AtomicUsize,
    dirs_done: AtomicUsize,
    dirs_total: usize,
    active: Mutex<Vec<String>>,
}

impl Progress<'_> {
    fn send(&self) {
        let current = self.active.lock().map(|a| a.join(", ")).unwrap_or_default();
        let _ = self.tx.send(AppEvent::ScanProgress {
            files: self.files.load(Ordering::Relaxed),
            dirs_done: self.dirs_done.load(Ordering::Relaxed),
            dirs_total: self.dirs_total,
            current,
        });
    }

    fn set_active(&self, name: &str, active: bool) {
        if let Ok(mut a) = self.active.lock() {
            if active {
                a.push(name.to_string());
            } else if let Some(i) = a.iter().position(|n| n == name) {
                a.remove(i);
            }
        }
    }
}

async fn run(
    client: &Client,
    db_path: PathBuf,
    drive_id: i64,
    tx: &UnboundedSender<AppEvent>,
) -> Result<Vec<String>> {
    let root = Progress {
        tx,
        files: AtomicUsize::new(0),
        dirs_done: AtomicUsize::new(0),
        dirs_total: 0,
        active: Mutex::new(vec!["/".into()]),
    };
    root.send();
    let mut files = Vec::new();
    list_all(
        client,
        drive_id,
        ROOT_DIR_ID,
        Depth::Child,
        &mut files,
        |n| {
            root.files.fetch_add(n, Ordering::Relaxed);
            root.send();
        },
    )
    .await?;

    let top_dirs: Vec<(i64, String)> = files
        .iter()
        .filter(|f| f.is_dir())
        .map(|f| (f.id, f.name.clone()))
        .collect();
    let total = top_dirs.len();
    let progress = Progress {
        tx,
        files: AtomicUsize::new(files.len()),
        dirs_done: AtomicUsize::new(0),
        dirs_total: total,
        active: Mutex::new(Vec::new()),
    };
    progress.send();
    let results: Vec<_> = stream::iter(top_dirs)
        .map(|(id, name)| {
            let progress = &progress;
            async move {
                progress.set_active(&name, true);
                progress.send();
                // On liste dans un tampon pour ne rien garder d'un dossier à moitié lu.
                let mut dir_files = Vec::new();
                let res = list_all(
                    client,
                    drive_id,
                    id,
                    Depth::Unlimited,
                    &mut dir_files,
                    |n| {
                        progress.files.fetch_add(n, Ordering::Relaxed);
                        progress.send();
                    },
                )
                .await;
                progress.set_active(&name, false);
                progress.dirs_done.fetch_add(1, Ordering::Relaxed);
                if res.is_err() {
                    progress.files.fetch_sub(dir_files.len(), Ordering::Relaxed);
                }
                progress.send();
                (id, name, res.map(|()| dir_files))
            }
        })
        .buffer_unordered(SCAN_CONCURRENCY)
        .collect()
        .await;

    let mut failed = Vec::new();
    for (id, name, res) in results {
        match res {
            Ok(dir_files) => files.extend(dir_files),
            Err(e) => failed.push((id, format!("/{name}"), format!("{e:#}"))),
        }
    }
    // L'ordre d'arrivée dépend du parallélisme : on le rend stable.
    failed.sort_by(|a, b| a.1.cmp(&b.1));
    if total > 0 && failed.len() == total {
        let list: Vec<_> = failed
            .iter()
            .map(|(_, name, e)| format!("{name} ({e})"))
            .collect();
        anyhow::bail!("aucun dossier n'a pu être scanné : {}", list.join(", "));
    }
    let _ = tx.send(AppEvent::ScanProgress {
        files: files.len(),
        dirs_done: total,
        dirs_total: total,
        current: "enregistrement dans le cache…".into(),
    });

    // Une même entrée peut apparaître deux fois si l'API renvoie des doublons entre pages.
    files.sort_by_key(|f| f.id);
    files.dedup_by_key(|f| f.id);

    let now = crate::now();
    let failed: Vec<FailedDir> = failed
        .into_iter()
        .map(|(id, name, e)| FailedDir {
            id,
            label: format!("{name} ({e})"),
        })
        .collect();
    tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        Db::open(&db_path)?.replace_tree(drive_id, &files, &failed, now)
    })
    .await?
}
