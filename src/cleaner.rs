use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::Client;
use crate::db::{Db, Entry, LogEntry};
use crate::events::AppEvent;
use crate::now;
use crate::rules::{self, Rule};

#[derive(Debug, Clone)]
pub struct PlannedDeletion {
    pub file_id: i64,
    pub path: String,
    pub version_id: i64,
    pub created_at: i64,
    pub size: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub rule: String,
    pub items: Vec<PlannedDeletion>,
    pub files_checked: usize,
    /// Fichiers ayant au moins une version à supprimer.
    pub files_affected: usize,
    pub total_size: i64,
    /// Fichiers dont les versions n'ont pas pu être chargées.
    pub errors: Vec<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ExecSummary {
    pub deleted: usize,
    pub failed: usize,
    pub freed: i64,
    pub cancelled: bool,
    pub errors: Vec<String>,
}

pub type Cancel = Arc<AtomicBool>;

/// Au-delà, une liste de versions en cache est rechargée depuis l'API.
const VERSIONS_TTL_SECS: i64 = 3600;

/// Versions d'un fichier, depuis le cache (si récent) ou l'API.
pub async fn load_versions(
    client: &Client,
    db: &mut Db,
    drive_id: i64,
    file_id: i64,
) -> Result<Vec<crate::api::Version>> {
    let now = now();
    if let Some(v) = db.cached_versions(drive_id, file_id, now - VERSIONS_TTL_SECS)? {
        return Ok(v);
    }
    let versions = client.list_versions(drive_id, file_id).await?;
    db.save_versions(drive_id, file_id, &versions, now)?;
    db.sync_version_count(drive_id, file_id, &versions)?;
    Ok(versions)
}

/// Charge les versions des fichiers ciblés et calcule ce que la règle supprimerait.
/// Ne supprime rien.
pub async fn prepare(
    client: Client,
    db_path: PathBuf,
    drive_id: i64,
    targets: Vec<Entry>,
    rule: Rule,
    tx: UnboundedSender<AppEvent>,
    cancel: Cancel,
) {
    let result = async {
        let mut db = Db::open(&db_path)?;
        let mut plan = Plan {
            rule: rule.label(),
            ..Default::default()
        };
        let targets: Vec<Entry> = targets
            .into_iter()
            // Pas de filtre sur nb_versions : le compte du scan peut être périmé.
            .filter(|e| !e.is_dir)
            .collect();
        let total = targets.len();
        let now = now();
        for (i, entry) in targets.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                plan.cancelled = true;
                break;
            }
            let _ = tx.send(AppEvent::PrepareProgress { done: i, total });
            let versions = match load_versions(&client, &mut db, drive_id, entry.id).await {
                Ok(v) => v,
                Err(e) => {
                    plan.errors.push(format!("{} : {e:#}", entry.path));
                    continue;
                }
            };
            plan.files_checked += 1;
            // Référence du scan relue dans le cache, qu'une suppression a pu ajuster.
            // Illisible : aucune référence, la plus récente reste protégée.
            let scan = db
                .entry(drive_id, entry.id)
                .ok()
                .flatten()
                .and_then(|e| e.scan());
            let selected = rules::select_versions(&versions, scan, &rule, now);
            if !selected.is_empty() {
                plan.files_affected += 1;
            }
            for v in selected {
                plan.total_size += v.size;
                plan.items.push(PlannedDeletion {
                    file_id: entry.id,
                    path: entry.path.clone(),
                    version_id: v.id,
                    created_at: v.created_at,
                    size: v.size,
                });
            }
        }
        let _ = tx.send(AppEvent::PrepareProgress { done: total, total });
        anyhow::Ok(plan)
    }
    .await;
    let _ = tx.send(AppEvent::PrepareDone(result.map_err(|e| format!("{e:#}"))));
}

/// Supprime les versions d'un plan validé, en journalisant chaque suppression.
pub async fn execute(
    client: Client,
    db_path: PathBuf,
    drive_id: i64,
    plan: Plan,
    tx: UnboundedSender<AppEvent>,
    cancel: Cancel,
) {
    let mut summary = ExecSummary::default();
    let mut db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            summary.errors.push(format!("{e:#}"));
            let _ = tx.send(AppEvent::ExecDone(summary));
            return;
        }
    };
    let total = plan.items.len();
    for item in &plan.items {
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        let res = client
            .delete_version(drive_id, item.file_id, item.version_id)
            .await;
        let error = res.as_ref().err().map(|e| format!("{e:#}"));
        let logged = db.log_deletion(&LogEntry {
            ts: now(),
            drive_id,
            file_id: item.file_id,
            path: item.path.clone(),
            version_id: item.version_id,
            version_created_at: item.created_at,
            size: item.size,
            ok: res.is_ok(),
            error: error.clone(),
        });
        if let Err(e) = logged {
            summary.errors.push(format!("journal : {e:#}"));
        }
        match error {
            None => {
                summary.deleted += 1;
                summary.freed += item.size;
                if let Err(e) =
                    db.remove_version(drive_id, item.file_id, item.version_id, item.size)
                {
                    summary.errors.push(format!("cache : {e:#}"));
                }
            }
            Some(e) => {
                summary.failed += 1;
                // L'état réel de la version est inconnu (déjà supprimée lors d'un essai
                // précédent, modifiée entre-temps…) : on recharge la liste au prochain usage.
                if let Err(e) = db.forget_versions(drive_id, item.file_id) {
                    summary.errors.push(format!("cache : {e:#}"));
                }
                summary
                    .errors
                    .push(format!("{} (version {}) : {e}", item.path, item.version_id));
            }
        }
        let _ = tx.send(AppEvent::ExecProgress {
            done: summary.deleted + summary.failed,
            total,
            freed: summary.freed,
            failed: summary.failed,
        });
    }
    if let Err(e) = db.recompute_aggregates(drive_id) {
        summary.errors.push(format!("cache : {e:#}"));
    }
    let _ = tx.send(AppEvent::ExecDone(summary));
}
