use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::api::{Drive, RemoteFile, Version};

/// Dossier que le scan n'a pas pu lister : son contenu en cache est conservé.
pub struct FailedDir {
    pub id: i64,
    /// Libellé affiché, par exemple `/Photos (HTTP 403 …)`.
    pub label: String,
}

pub struct Db {
    conn: Connection,
}

/// Un fichier ou dossier du cache. Pour un dossier, `size`, `versions_size` et
/// `versioned_files` sont agrégés sur toute sa descendance.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub parent_id: i64,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: i64,
    pub nb_versions: i64,
    /// Espace occupé par les anciennes versions (taille totale - taille courante).
    pub versions_size: i64,
    pub versioned_files: i64,
    pub last_modified_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct DriveInfo {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub used_size: i64,
    pub scanned_at: Option<i64>,
    /// Dossiers non rescannés lors du dernier scan (contenu conservé de l'avant-dernier).
    pub scan_warning: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub files: i64,
    pub dirs: i64,
    pub files_size: i64,
    pub versioned_files: i64,
    pub old_versions: i64,
    pub versions_size: i64,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub text: String,
    pub extension: String,
    pub min_size: i64,
    pub min_versions: i64,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts: i64,
    pub drive_id: i64,
    pub file_id: i64,
    pub path: String,
    pub version_id: i64,
    pub version_created_at: i64,
    pub size: i64,
    pub ok: bool,
    pub error: Option<String>,
}

const ENTRY_COLS: &str = "id, parent_id, name, path, is_dir, size, nb_versions, versions_size, versioned_files, last_modified_at";

fn entry_from_row(r: &Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        name: r.get(2)?,
        path: r.get(3)?,
        is_dir: r.get(4)?,
        size: r.get(5)?,
        nb_versions: r.get(6)?,
        versions_size: r.get(7)?,
        versioned_files: r.get(8)?,
        last_modified_at: r.get(9)?,
    })
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// Transaction qui prend le verrou d'écriture dès le début. En mode WAL, une
    /// transaction différée qui lit puis écrit échoue aussitôt (SQLITE_BUSY_SNAPSHOT,
    /// sans busy_timeout) si une autre connexion a écrit entre-temps.
    fn write_tx(&mut self) -> Result<Transaction<'_>> {
        Ok(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        Self::create_schema(&conn)?;
        let has_warning: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('drives') WHERE name = 'scan_warning'",
            [],
            |r| r.get(0),
        )?;
        if !has_warning {
            conn.execute("ALTER TABLE drives ADD COLUMN scan_warning TEXT", [])?;
        }
        Ok(Self { conn })
    }

    fn create_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS drives(
                id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0, used_size INTEGER NOT NULL DEFAULT 0,
                scanned_at INTEGER);
             CREATE TABLE IF NOT EXISTS files(
                drive_id INTEGER NOT NULL, id INTEGER NOT NULL, parent_id INTEGER NOT NULL,
                name TEXT NOT NULL, path TEXT NOT NULL, is_dir INTEGER NOT NULL,
                size INTEGER NOT NULL, nb_versions INTEGER NOT NULL, versions_size INTEGER NOT NULL,
                versioned_files INTEGER NOT NULL, last_modified_at INTEGER NOT NULL,
                PRIMARY KEY(drive_id, id));
             CREATE INDEX IF NOT EXISTS files_parent ON files(drive_id, parent_id);
             CREATE INDEX IF NOT EXISTS files_vsize ON files(drive_id, versions_size);
             CREATE TABLE IF NOT EXISTS versions(
                drive_id INTEGER NOT NULL, file_id INTEGER NOT NULL, id INTEGER NOT NULL,
                size INTEGER NOT NULL, created_at INTEGER NOT NULL, keep_forever INTEGER NOT NULL,
                PRIMARY KEY(drive_id, file_id, id));
             CREATE TABLE IF NOT EXISTS versions_fetched(
                drive_id INTEGER NOT NULL, file_id INTEGER NOT NULL, fetched_at INTEGER NOT NULL,
                PRIMARY KEY(drive_id, file_id));
             CREATE TABLE IF NOT EXISTS deletions(
                rowid INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                drive_id INTEGER NOT NULL, file_id INTEGER NOT NULL, path TEXT NOT NULL,
                version_id INTEGER NOT NULL, version_created_at INTEGER NOT NULL,
                size INTEGER NOT NULL, ok INTEGER NOT NULL, error TEXT);",
        )?;
        Ok(())
    }

    // ---- drives ----

    pub fn upsert_drives(&self, drives: &[Drive]) -> Result<()> {
        for d in drives {
            self.conn.execute(
                "INSERT INTO drives(id, name, size, used_size) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, size = ?3, used_size = ?4",
                params![d.id, d.name, d.size.unwrap_or(0), d.used_size.unwrap_or(0)],
            )?;
        }
        Ok(())
    }

    pub fn drive(&self, drive_id: i64) -> Result<Option<DriveInfo>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, size, used_size, scanned_at, scan_warning FROM drives WHERE id = ?1",
                [drive_id],
                |r| {
                    Ok(DriveInfo {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        size: r.get(2)?,
                        used_size: r.get(3)?,
                        scanned_at: r.get(4)?,
                        scan_warning: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    // ---- scan ----

    /// Remplace l'arborescence d'un drive par le résultat d'un scan, en une seule
    /// transaction (arbre, agrégats et avertissement).
    ///
    /// Le contenu en cache des dossiers de `failed` (que le scan n'a pas pu lister)
    /// est conservé sous leur chemin actuel. Renvoie, pour chacun, le message
    /// mémorisé dans `drives.scan_warning`.
    pub fn replace_tree(
        &mut self,
        drive_id: i64,
        files: &[RemoteFile],
        failed: &[FailedDir],
        now: i64,
    ) -> Result<Vec<String>> {
        let paths = build_paths(files);
        let tx = self.write_tx()?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS temp.kept;
             DROP TABLE IF EXISTS temp.fresh;
             CREATE TEMP TABLE kept AS SELECT * FROM files WHERE 0;
             CREATE TEMP TABLE fresh(id INTEGER PRIMARY KEY);",
        )?;
        if !failed.is_empty() {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO fresh(id) VALUES (?1)")?;
            for f in files {
                stmt.execute([f.id])?;
            }
        }
        let mut report = Vec::new();
        for dir in failed {
            let old: Option<String> = tx
                .query_row(
                    "SELECT path FROM files WHERE drive_id = ?1 AND id = ?2",
                    params![drive_id, dir.id],
                    |r| r.get(0),
                )
                .optional()?;
            let mut n = 0;
            if let Some(old) = old {
                // CROSS JOIN impose à SQLite de partir de `sub` et d'utiliser l'index
                // sur parent_id ; sinon il parcourt `files` à chaque étape (quadratique).
                // On descend l'ancien arbre par parent_id, sans entrer dans ce que le
                // scan vient de relire : un élément déplacé ailleurs depuis n'est pas
                // ramené dans ce dossier. Le dossier a pu être renommé : le contenu
                // conservé reprend son nouveau chemin, sinon recherche et nettoyage par
                // préfixe viseraient les mauvais fichiers.
                let new = paths.get(&dir.id).cloned().unwrap_or_else(|| old.clone());
                n = tx.execute(
                    "WITH RECURSIVE sub(id) AS (
                        SELECT id FROM files
                        WHERE drive_id = ?1 AND parent_id = ?2 AND id NOT IN (SELECT id FROM fresh)
                        UNION
                        SELECT f.id FROM sub CROSS JOIN files f
                        ON f.drive_id = ?1 AND f.parent_id = sub.id
                        WHERE f.id NOT IN (SELECT id FROM fresh))
                     INSERT INTO kept SELECT drive_id, id, parent_id, name,
                        CASE WHEN substr(path, 1, length(?3)) = ?3
                             THEN ?4 || substr(path, length(?3)) ELSE path END,
                        is_dir, size, nb_versions, versions_size, versioned_files, last_modified_at
                     FROM files WHERE drive_id = ?1 AND id IN (SELECT id FROM sub)
                        AND id NOT IN (SELECT id FROM kept)",
                    params![drive_id, dir.id, format!("{old}/"), new],
                )?;
            }
            // Un dossier jamais scanné n'a aucun ancien contenu : il apparaît vide.
            report.push(if n > 0 {
                format!("{} ; ancien contenu conservé", dir.label)
            } else {
                format!("{} ; aucun contenu en cache, affiché vide", dir.label)
            });
        }
        tx.execute("DELETE FROM files WHERE drive_id = ?1", [drive_id])?;
        tx.execute(
            "DELETE FROM versions WHERE drive_id = ?1 AND file_id NOT IN (SELECT id FROM kept)",
            [drive_id],
        )?;
        tx.execute(
            "DELETE FROM versions_fetched WHERE drive_id = ?1 AND file_id NOT IN (SELECT id FROM kept)",
            [drive_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO files(drive_id, id, parent_id, name, path, is_dir, size,
                    nb_versions, versions_size, versioned_files, last_modified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)",
            )?;
            for f in files {
                let size = f.size.unwrap_or(0);
                let (nb, vsize) = match &f.version {
                    Some(v) if v.number > 1 => {
                        (v.number, (v.total_size.unwrap_or(size) - size).max(0))
                    }
                    Some(v) => (v.number, 0),
                    None => (0, 0),
                };
                stmt.execute(params![
                    drive_id,
                    f.id,
                    f.parent_id.unwrap_or(crate::api::ROOT_DIR_ID),
                    f.name,
                    paths
                        .get(&f.id)
                        .cloned()
                        .unwrap_or_else(|| format!("/{}", f.name)),
                    f.is_dir(),
                    if f.is_dir() { 0 } else { size },
                    nb,
                    vsize,
                    f.last_modified_at.unwrap_or(0),
                ])?;
            }
        }
        tx.execute_batch(
            "INSERT OR IGNORE INTO files SELECT * FROM kept;
             DROP TABLE temp.kept;
             DROP TABLE temp.fresh;",
        )?;
        let warning = (!report.is_empty()).then(|| report.join(", "));
        tx.execute(
            "UPDATE drives SET scanned_at = ?2, scan_warning = ?3 WHERE id = ?1",
            params![drive_id, now, warning],
        )?;
        aggregates(&tx, drive_id)?;
        tx.commit()?;
        Ok(report)
    }

    /// Recalcule les totaux des dossiers à partir des fichiers.
    pub fn recompute_aggregates(&mut self, drive_id: i64) -> Result<()> {
        let tx = self.write_tx()?;
        aggregates(&tx, drive_id)?;
        tx.commit()?;
        Ok(())
    }

    // ---- lecture ----

    pub fn stats(&self, drive_id: i64) -> Result<Stats> {
        Ok(self.conn.query_row(
            "SELECT
                COALESCE(SUM(NOT is_dir), 0), COALESCE(SUM(is_dir), 0),
                COALESCE(SUM(CASE WHEN is_dir THEN 0 ELSE size END), 0),
                COALESCE(SUM(NOT is_dir AND nb_versions > 1), 0),
                COALESCE(SUM(CASE WHEN NOT is_dir AND nb_versions > 1 THEN nb_versions - 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_dir THEN 0 ELSE versions_size END), 0)
             FROM files WHERE drive_id = ?1",
            [drive_id],
            |r| {
                Ok(Stats {
                    files: r.get(0)?,
                    dirs: r.get(1)?,
                    files_size: r.get(2)?,
                    versioned_files: r.get(3)?,
                    old_versions: r.get(4)?,
                    versions_size: r.get(5)?,
                })
            },
        )?)
    }

    pub fn entry(&self, drive_id: i64, id: i64) -> Result<Option<Entry>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ENTRY_COLS} FROM files WHERE drive_id = ?1 AND id = ?2"),
                params![drive_id, id],
                entry_from_row,
            )
            .optional()?)
    }

    fn entries(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, entry_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Enfants directs d'un dossier : dossiers d'abord, puis par poids des versions.
    pub fn children(&self, drive_id: i64, parent_id: i64) -> Result<Vec<Entry>> {
        self.entries(
            &format!(
                "SELECT {ENTRY_COLS} FROM files WHERE drive_id = ?1 AND parent_id = ?2
                 ORDER BY is_dir DESC, versions_size DESC, name COLLATE NOCASE"
            ),
            params![drive_id, parent_id],
        )
    }

    pub fn top(&self, drive_id: i64, limit: i64) -> Result<Vec<Entry>> {
        self.entries(
            &format!(
                "SELECT {ENTRY_COLS} FROM files
                 WHERE drive_id = ?1 AND NOT is_dir AND nb_versions > 1
                 ORDER BY versions_size DESC, nb_versions DESC LIMIT ?2"
            ),
            params![drive_id, limit],
        )
    }

    pub fn search(&self, drive_id: i64, f: &SearchFilter, limit: i64) -> Result<Vec<Entry>> {
        let ext = f.extension.trim().trim_start_matches('.');
        let ext_pattern = if ext.is_empty() {
            "%".to_string()
        } else {
            format!("%.{}", like_escape(ext))
        };
        self.entries(
            &format!(
                "SELECT {ENTRY_COLS} FROM files
                 WHERE drive_id = ?1 AND NOT is_dir
                   AND name LIKE ?2 ESCAPE '\\' AND name LIKE ?3 ESCAPE '\\'
                   AND size >= ?4 AND nb_versions >= ?5
                 ORDER BY versions_size DESC, nb_versions DESC LIMIT ?6"
            ),
            params![
                drive_id,
                format!("%{}%", like_escape(f.text.trim())),
                ext_pattern,
                f.min_size,
                f.min_versions,
                limit
            ],
        )
    }

    /// Fichiers versionnés d'un dossier, récursivement.
    pub fn versioned_descendants(&self, drive_id: i64, dir: &Entry) -> Result<Vec<Entry>> {
        // Comparaison exacte : LIKE ignore la casse et inclurait « /docs » pour « /Docs ».
        let prefix = if dir.id == crate::api::ROOT_DIR_ID {
            "/".to_string()
        } else {
            format!("{}/", dir.path)
        };
        self.entries(
            &format!(
                "SELECT {ENTRY_COLS} FROM files
                 WHERE drive_id = ?1 AND NOT is_dir AND nb_versions > 1
                   AND substr(path, 1, length(?2)) = ?2
                 ORDER BY versions_size DESC"
            ),
            params![drive_id, prefix],
        )
    }

    // ---- versions ----

    pub fn save_versions(
        &mut self,
        drive_id: i64,
        file_id: i64,
        versions: &[Version],
        now: i64,
    ) -> Result<()> {
        let tx = self.write_tx()?;
        tx.execute(
            "DELETE FROM versions WHERE drive_id = ?1 AND file_id = ?2",
            params![drive_id, file_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO versions(drive_id, file_id, id, size, created_at, keep_forever)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for v in versions {
                stmt.execute(params![
                    drive_id,
                    file_id,
                    v.id,
                    v.size,
                    v.created_at,
                    v.keep_forever
                ])?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO versions_fetched(drive_id, file_id, fetched_at) VALUES (?1, ?2, ?3)",
            params![drive_id, file_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Versions en cache, `None` si elles n'ont jamais été chargées ou l'ont été
    /// avant `fetched_after`.
    pub fn cached_versions(
        &self,
        drive_id: i64,
        file_id: i64,
        fetched_after: i64,
    ) -> Result<Option<Vec<Version>>> {
        let fetched: Option<i64> = self
            .conn
            .query_row(
                "SELECT fetched_at FROM versions_fetched WHERE drive_id = ?1 AND file_id = ?2",
                params![drive_id, file_id],
                |r| r.get(0),
            )
            .optional()?;
        if fetched.is_none_or(|ts| ts < fetched_after) {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, size, created_at, keep_forever FROM versions
             WHERE drive_id = ?1 AND file_id = ?2 ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![drive_id, file_id], |r| {
            Ok(Version {
                id: r.get(0)?,
                size: r.get(1)?,
                created_at: r.get(2)?,
                keep_forever: r.get(3)?,
            })
        })?;
        Ok(Some(rows.collect::<rusqlite::Result<_>>()?))
    }

    /// Aligne le compte de versions d'un fichier sur une liste fraîchement chargée,
    /// le scan ayant pu en voir moins (versions ajoutées depuis). Les totaux des
    /// dossiers parents sont ajustés d'autant.
    pub fn sync_version_count(
        &mut self,
        drive_id: i64,
        file_id: i64,
        versions: &[Version],
    ) -> Result<()> {
        let current = crate::rules::current_version_id(versions);
        let old_size: i64 = versions
            .iter()
            .filter(|v| Some(v.id) != current)
            .map(|v| v.size)
            .sum();
        let count = versions.len() as i64;
        let tx = self.write_tx()?;
        let before: Option<(i64, i64)> = tx
            .query_row(
                "SELECT nb_versions, versions_size FROM files
                 WHERE drive_id = ?1 AND id = ?2 AND NOT is_dir",
                params![drive_id, file_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((nb, vsize)) = before.filter(|(nb, _)| *nb < count) else {
            return Ok(());
        };
        let new_vsize = vsize.max(old_size);
        tx.execute(
            "UPDATE files SET nb_versions = ?3, versions_size = ?4, versioned_files = (?3 > 1)
             WHERE drive_id = ?1 AND id = ?2",
            params![drive_id, file_id, count, new_vsize],
        )?;
        let delta_size = new_vsize - vsize;
        let delta_versioned = i64::from(nb <= 1 && count > 1);
        if delta_size != 0 || delta_versioned != 0 {
            // UNION (et non UNION ALL) arrête la remontée en cas de cycle.
            tx.execute(
                "WITH RECURSIVE anc(id) AS (
                    SELECT parent_id FROM files WHERE drive_id = ?1 AND id = ?2
                    UNION
                    SELECT f.parent_id FROM files f JOIN anc ON f.drive_id = ?1 AND f.id = anc.id)
                 UPDATE files SET versions_size = versions_size + ?3,
                    versioned_files = versioned_files + ?4
                 WHERE drive_id = ?1 AND is_dir AND id IN (SELECT id FROM anc)",
                params![drive_id, file_id, delta_size, delta_versioned],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Met à jour le cache après la suppression d'une version.
    pub fn remove_version(
        &self,
        drive_id: i64,
        file_id: i64,
        version_id: i64,
        size: i64,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM versions WHERE drive_id = ?1 AND file_id = ?2 AND id = ?3",
            params![drive_id, file_id, version_id],
        )?;
        self.conn.execute(
            "UPDATE files SET nb_versions = MAX(nb_versions - 1, 1),
                versions_size = MAX(versions_size - ?3, 0)
             WHERE drive_id = ?1 AND id = ?2",
            params![drive_id, file_id, size],
        )?;
        Ok(())
    }

    /// Oublie les versions en cache d'un fichier : elles seront rechargées depuis l'API.
    pub fn forget_versions(&self, drive_id: i64, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM versions WHERE drive_id = ?1 AND file_id = ?2",
            params![drive_id, file_id],
        )?;
        self.conn.execute(
            "DELETE FROM versions_fetched WHERE drive_id = ?1 AND file_id = ?2",
            params![drive_id, file_id],
        )?;
        Ok(())
    }

    // ---- journal ----

    pub fn log_deletion(&self, e: &LogEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO deletions(ts, drive_id, file_id, path, version_id, version_created_at, size, ok, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                e.ts,
                e.drive_id,
                e.file_id,
                e.path,
                e.version_id,
                e.version_created_at,
                e.size,
                e.ok,
                e.error
            ],
        )?;
        Ok(())
    }

    pub fn log(&self, drive_id: i64) -> Result<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, drive_id, file_id, path, version_id, version_created_at, size, ok, error
             FROM deletions WHERE drive_id = ?1 ORDER BY rowid DESC",
        )?;
        let rows = stmt.query_map([drive_id], |r| {
            Ok(LogEntry {
                ts: r.get(0)?,
                drive_id: r.get(1)?,
                file_id: r.get(2)?,
                path: r.get(3)?,
                version_id: r.get(4)?,
                version_created_at: r.get(5)?,
                size: r.get(6)?,
                ok: r.get(7)?,
                error: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Reconstruit les chemins complets à partir des liens parent/enfant.
/// Recalcule les totaux des dossiers, dans la transaction de l'appelant.
fn aggregates(conn: &Connection, drive_id: i64) -> Result<()> {
    struct Node {
        parent: i64,
        is_dir: bool,
        size: i64,
        vsize: i64,
        versioned: i64,
    }
    let mut nodes: HashMap<i64, Node> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, is_dir, size, versions_size, nb_versions
             FROM files WHERE drive_id = ?1",
        )?;
        let rows = stmt.query_map([drive_id], |r| {
            let is_dir: bool = r.get(2)?;
            Ok((
                r.get::<_, i64>(0)?,
                Node {
                    parent: r.get(1)?,
                    is_dir,
                    size: if is_dir { 0 } else { r.get(3)? },
                    vsize: if is_dir { 0 } else { r.get(4)? },
                    versioned: if !is_dir && r.get::<_, i64>(5)? > 1 {
                        1
                    } else {
                        0
                    },
                },
            ))
        })?;
        for row in rows {
            let (id, n) = row?;
            nodes.insert(id, n);
        }
    }
    let mut totals: HashMap<i64, (i64, i64, i64)> = HashMap::new();
    for n in nodes.values().filter(|n| !n.is_dir) {
        let mut parent = n.parent;
        // Garde-fou contre un cycle éventuel dans les données, borné comme
        // `build_paths` pour qu'un cycle ne coûte pas n² tours.
        for _ in 0..256 {
            let Some(p) = nodes.get(&parent) else { break };
            let t = totals.entry(parent).or_default();
            t.0 += n.size;
            t.1 += n.vsize;
            t.2 += n.versioned;
            parent = p.parent;
        }
    }
    {
        let mut stmt = conn.prepare(
            "UPDATE files SET size = ?3, versions_size = ?4, versioned_files = ?5
             WHERE drive_id = ?1 AND id = ?2",
        )?;
        for id in nodes.iter().filter(|(_, n)| n.is_dir).map(|(id, _)| id) {
            let (s, v, c) = totals.get(id).copied().unwrap_or_default();
            stmt.execute(params![drive_id, id, s, v, c])?;
        }
        conn.execute(
            "UPDATE files SET versioned_files = (nb_versions > 1) WHERE drive_id = ?1 AND NOT is_dir",
            [drive_id],
        )?;
    }
    Ok(())
}

fn build_paths(files: &[RemoteFile]) -> HashMap<i64, String> {
    let by_id: HashMap<i64, &RemoteFile> = files.iter().map(|f| (f.id, f)).collect();
    let mut paths: HashMap<i64, String> = HashMap::new();
    for f in files {
        let mut parts = vec![f.name.as_str()];
        let mut parent = f.parent_id;
        let mut guard = 0;
        while let Some(p) = parent.and_then(|id| by_id.get(&id)) {
            parts.push(&p.name);
            parent = p.parent_id;
            guard += 1;
            if guard > 256 {
                break;
            }
        }
        parts.reverse();
        paths.insert(f.id, format!("/{}", parts.join("/")));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::FileVersionInfo;

    fn file(
        id: i64,
        parent: i64,
        name: &str,
        dir: bool,
        size: i64,
        versions: Option<(i64, i64)>,
    ) -> RemoteFile {
        RemoteFile {
            id,
            name: name.into(),
            kind: Some(if dir { "dir" } else { "file" }.into()),
            parent_id: Some(parent),
            size: Some(size),
            last_modified_at: Some(0),
            status: None,
            version: versions.map(|(number, total)| FileVersionInfo {
                number,
                total_size: Some(total),
                keep_forever: false,
            }),
        }
    }

    fn failed(id: i64, label: &str) -> FailedDir {
        FailedDir {
            id,
            label: label.into(),
        }
    }

    fn sample() -> Db {
        let mut db = Db::open_in_memory().unwrap();
        db.upsert_drives(&[Drive {
            id: 7,
            name: "d".into(),
            size: None,
            used_size: None,
        }])
        .unwrap();
        let files = vec![
            file(10, 1, "Docs", true, 0, None),
            file(11, 10, "Sub_dir", true, 0, None),
            file(20, 10, "a.docx", false, 100, Some((3, 400))),
            file(21, 11, "b.xlsx", false, 50, Some((2, 120))),
            file(22, 1, "c.txt", false, 10, Some((1, 10))),
        ];
        db.replace_tree(7, &files, &[], 1).unwrap();
        db
    }

    #[test]
    fn paths_and_aggregates() {
        let db = sample();
        let b = db.entry(7, 21).unwrap().unwrap();
        assert_eq!(b.path, "/Docs/Sub_dir/b.xlsx");
        assert_eq!(b.versions_size, 70);
        let docs = db.entry(7, 10).unwrap().unwrap();
        assert_eq!(docs.versions_size, 300 + 70);
        assert_eq!(docs.versioned_files, 2);
        assert_eq!(docs.size, 150);
        let s = db.stats(7).unwrap();
        assert_eq!(
            (
                s.files,
                s.dirs,
                s.versioned_files,
                s.old_versions,
                s.versions_size
            ),
            (3, 2, 2, 3, 370)
        );
    }

    #[test]
    fn queries() {
        let db = sample();
        assert_eq!(
            db.top(7, 10)
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![20, 21]
        );
        let sub = db.entry(7, 11).unwrap().unwrap();
        assert_eq!(db.versioned_descendants(7, &sub).unwrap().len(), 1);
        let root = Entry {
            id: 1,
            parent_id: 0,
            name: String::new(),
            path: "/".into(),
            is_dir: true,
            size: 0,
            nb_versions: 0,
            versions_size: 0,
            versioned_files: 0,
            last_modified_at: 0,
        };
        assert_eq!(db.versioned_descendants(7, &root).unwrap().len(), 2);
        // Un dossier qui ne diffère que par la casse n'est pas un descendant.
        let mut docs = db.entry(7, 10).unwrap().unwrap();
        docs.path = "/docs".into();
        assert!(db.versioned_descendants(7, &docs).unwrap().is_empty());
        let f = SearchFilter {
            extension: ".xlsx".into(),
            ..Default::default()
        };
        assert_eq!(db.search(7, &f, 10).unwrap()[0].id, 21);
        // « _ » n'est pas un joker.
        let f = SearchFilter {
            text: "_".into(),
            ..Default::default()
        };
        assert!(db.search(7, &f, 10).unwrap().is_empty());
    }

    #[test]
    fn partial_scan_keeps_failed_dirs() {
        let mut db = sample();
        db.save_versions(
            7,
            21,
            &[Version {
                id: 1,
                size: 1,
                created_at: 1,
                keep_forever: false,
            }],
            5,
        )
        .unwrap();
        // Rescan où /Docs (10) n'a pas pu être listé : seule la racine est revue.
        let files = vec![
            file(10, 1, "Docs", true, 0, None),
            file(22, 1, "c.txt", false, 10, Some((1, 10))),
        ];
        db.replace_tree(7, &files, &[failed(10, "/Docs (403)")], 2)
            .unwrap();
        // Contenu de /Docs conservé, agrégats recalculés, avertissement mémorisé.
        assert!(db.entry(7, 21).unwrap().is_some());
        assert_eq!(db.entry(7, 10).unwrap().unwrap().versions_size, 370);
        assert!(db.cached_versions(7, 21, 0).unwrap().is_some());
        let d = db.drive(7).unwrap().unwrap();
        assert_eq!(
            d.scan_warning.as_deref(),
            Some("/Docs (403) ; ancien contenu conservé")
        );
        // Un scan complet efface l'avertissement et le contenu disparu.
        db.replace_tree(7, &files, &[], 3).unwrap();
        assert!(db.entry(7, 21).unwrap().is_none());
        assert!(db.drive(7).unwrap().unwrap().scan_warning.is_none());
    }

    #[test]
    fn partial_scan_follows_renamed_failed_dir() {
        let mut db = sample();
        // /Docs (10) renommé en /Archive, un nouveau /Docs (12) créé ; /Archive échoue.
        let files = vec![
            file(10, 1, "Archive", true, 0, None),
            file(12, 1, "Docs", true, 0, None),
        ];
        let report = db
            .replace_tree(7, &files, &[failed(10, "/Archive"), failed(12, "/Docs")], 2)
            .unwrap();
        // /Docs (12) n'avait rien en cache : il n'est pas annoncé comme conservé.
        assert_eq!(
            report,
            vec![
                "/Archive ; ancien contenu conservé",
                "/Docs ; aucun contenu en cache, affiché vide"
            ]
        );
        assert_eq!(db.entry(7, 20).unwrap().unwrap().path, "/Archive/a.docx");
        assert_eq!(
            db.entry(7, 21).unwrap().unwrap().path,
            "/Archive/Sub_dir/b.xlsx"
        );
        let archive = db.entry(7, 10).unwrap().unwrap();
        let docs = db.entry(7, 12).unwrap().unwrap();
        let ids = |e| {
            db.versioned_descendants(7, &e)
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(docs), Vec::<i64>::new());
        let mut a = ids(archive);
        a.sort();
        assert_eq!(a, vec![20, 21]);
    }

    #[test]
    fn partial_scan_does_not_pull_back_moved_content() {
        // Ancien arbre : /Docs/Sub_dir/b.xlsx. Sub_dir (11) a été déplacé à la racine.
        let moved = || {
            vec![
                file(10, 1, "Docs", true, 0, None),
                file(11, 1, "Sub_dir", true, 0, None),
            ]
        };

        // A : /Docs et /Sub_dir échouent tous les deux.
        let mut db = sample();
        db.replace_tree(
            7,
            &moved(),
            &[failed(10, "/Docs"), failed(11, "/Sub_dir")],
            2,
        )
        .unwrap();
        let b = db.entry(7, 21).unwrap().unwrap();
        assert_eq!((b.parent_id, b.path.as_str()), (11, "/Sub_dir/b.xlsx"));
        let ids = |db: &Db, id| {
            let dir = db.entry(7, id).unwrap().unwrap();
            db.versioned_descendants(7, &dir)
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&db, 10), vec![20]);
        assert_eq!(ids(&db, 11), vec![21]);

        // B : seul /Docs échoue ; /Sub_dir, rescanné, est vide (b.xlsx supprimé).
        let mut db = sample();
        let report = db
            .replace_tree(7, &moved(), &[failed(10, "/Docs")], 2)
            .unwrap();
        assert!(db.entry(7, 21).unwrap().is_none(), "pas de fantôme");
        assert!(db.children(7, 11).unwrap().is_empty());
        assert_eq!(db.entry(7, 11).unwrap().unwrap().versions_size, 0);
        assert_eq!(report, vec!["/Docs ; ancien contenu conservé"]);

        // Tout le contenu est parti ailleurs : rien n'est annoncé comme conservé.
        let mut db = sample();
        let files = vec![
            file(10, 1, "Docs", true, 0, None),
            file(11, 1, "Sub_dir", true, 0, None),
            file(20, 1, "a.docx", false, 100, Some((3, 400))),
        ];
        let report = db
            .replace_tree(7, &files, &[failed(10, "/Docs")], 2)
            .unwrap();
        assert_eq!(report, vec!["/Docs ; aucun contenu en cache, affiché vide"]);
    }

    /// Gros dossier en échec : 20 000 éléments sur 4 niveaux.
    fn big_tree() -> Vec<RemoteFile> {
        let mut files = vec![file(10, 1, "Docs", true, 0, None)];
        let mut id = 100;
        for a in 0..20 {
            let da = id;
            files.push(file(da, 10, &format!("a{a}"), true, 0, None));
            id += 1;
            for b in 0..10 {
                let db_ = id;
                files.push(file(db_, da, &format!("b{b}"), true, 0, None));
                id += 1;
                for c in 0..100 {
                    files.push(file(id, db_, &format!("f{c}"), false, 10, Some((2, 30))));
                    id += 1;
                }
            }
        }
        files
    }

    #[test]
    fn partial_scan_of_big_failed_dir_is_fast() {
        let mut db = Db::open_in_memory().unwrap();
        db.upsert_drives(&[Drive {
            id: 7,
            name: "d".into(),
            size: None,
            used_size: None,
        }])
        .unwrap();
        db.replace_tree(7, &big_tree(), &[], 1).unwrap();
        let started = std::time::Instant::now();
        let report = db
            .replace_tree(
                7,
                &[file(10, 1, "Docs", true, 0, None)],
                &[failed(10, "/Docs")],
                2,
            )
            .unwrap();
        // En descente quadratique, ce cas prenait plusieurs minutes.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(report, vec!["/Docs ; ancien contenu conservé"]);
        assert_eq!(db.stats(7).unwrap().files, 20_000);
    }

    #[test]
    fn partial_scan_survives_concurrent_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let mut db = Db::open(&path).unwrap();
        db.upsert_drives(&[Drive {
            id: 7,
            name: "d".into(),
            size: None,
            used_size: None,
        }])
        .unwrap();
        db.replace_tree(7, &big_tree(), &[], 1).unwrap();

        // Une autre connexion écrit en continu (chargement de versions pendant un scan).
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = std::thread::spawn({
            let (path, stop) = (path.clone(), stop.clone());
            move || {
                let mut other = Db::open(&path).unwrap();
                let mut i = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let v = Version {
                        id: i,
                        size: 1,
                        created_at: i,
                        keep_forever: false,
                    };
                    other.save_versions(7, 999_999, &[v], i).unwrap();
                    i += 1;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        });
        for n in 0..5 {
            db.replace_tree(
                7,
                &[file(10, 1, "Docs", true, 0, None)],
                &[failed(10, "/Docs")],
                n,
            )
            .unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn version_cache_expiry_and_count_sync() {
        let mut db = sample();
        // c.txt : le scan n'a vu qu'une version.
        let vs = vec![
            Version {
                id: 1,
                size: 4,
                created_at: 1,
                keep_forever: false,
            },
            Version {
                id: 2,
                size: 10,
                created_at: 2,
                keep_forever: false,
            },
        ];
        db.save_versions(7, 22, &vs, 100).unwrap();
        assert!(db.cached_versions(7, 22, 100).unwrap().is_some());
        assert!(db.cached_versions(7, 22, 101).unwrap().is_none());
        db.sync_version_count(7, 22, &vs).unwrap();
        let e = db.entry(7, 22).unwrap().unwrap();
        assert_eq!(
            (e.nb_versions, e.versions_size, e.versioned_files),
            (2, 4, 1)
        );
        // Une liste plus courte ne fait pas baisser le compte.
        db.sync_version_count(7, 22, &vs[..1]).unwrap();
        assert_eq!(db.entry(7, 22).unwrap().unwrap().nb_versions, 2);
        // b.xlsx (Docs/Sub_dir) : 3 versions au lieu de 2, les parents suivent.
        let vs: Vec<Version> = (1..=3)
            .map(|i| Version {
                id: i,
                size: 50,
                created_at: i,
                keep_forever: false,
            })
            .collect();
        db.sync_version_count(7, 21, &vs).unwrap();
        assert_eq!(db.entry(7, 21).unwrap().unwrap().versions_size, 100);
        assert_eq!(db.entry(7, 11).unwrap().unwrap().versions_size, 100);
        let docs = db.entry(7, 10).unwrap().unwrap();
        assert_eq!((docs.versions_size, docs.versioned_files), (400, 2));
    }

    #[test]
    fn version_cache_and_removal() {
        let mut db = sample();
        assert!(db.cached_versions(7, 20, 0).unwrap().is_none());
        let vs = vec![
            Version {
                id: 1,
                size: 100,
                created_at: 1,
                keep_forever: false,
            },
            Version {
                id: 2,
                size: 200,
                created_at: 2,
                keep_forever: false,
            },
        ];
        db.save_versions(7, 20, &vs, 5).unwrap();
        assert_eq!(db.cached_versions(7, 20, 0).unwrap().unwrap()[0].id, 2);
        db.remove_version(7, 20, 1, 100).unwrap();
        db.recompute_aggregates(7).unwrap();
        let e = db.entry(7, 20).unwrap().unwrap();
        assert_eq!((e.nb_versions, e.versions_size), (2, 200));
        assert_eq!(db.entry(7, 10).unwrap().unwrap().versions_size, 270);
        db.forget_versions(7, 20).unwrap();
        assert!(db.cached_versions(7, 20, 0).unwrap().is_none());
    }
}
