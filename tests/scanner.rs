mod common;

use common::*;
use kdvc::db::Db;
use kdvc::events::AppEvent;
use serde_json::json;
use tokio::sync::mpsc::unbounded_channel;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn listing(data: serde_json::Value, cursor: Option<&str>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "result": "success", "data": data, "cursor": cursor, "has_more": cursor.is_some()
    }))
}

async fn run_scan(server: &MockServer, db_path: &std::path::Path) -> Result<Vec<String>, String> {
    let (tx, mut rx) = unbounded_channel();
    kdvc::scanner::scan(client(server), db_path.to_path_buf(), DRIVE, tx).await;
    let mut progressed = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AppEvent::ScanProgress { .. } => progressed = true,
            AppEvent::ScanDone(res) => {
                assert!(progressed, "la progression doit être signalée");
                return res;
            }
            _ => {}
        }
    }
    panic!("pas de ScanDone");
}

async fn mount_root(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/3/drive/{DRIVE}/files/1/files")))
        .and(query_param("depth", "child"))
        .respond_with(listing(
            json!([
                {"id": 10, "name": "Docs", "type": "dir", "parent_id": 1},
                {"id": 11, "name": "Photos", "type": "dir", "parent_id": 1},
                {"id": 30, "name": "notes.md", "type": "file", "parent_id": 1, "size": 100,
                 "version": {"number": 2, "total_size": 200, "keep_forever": false}},
                {"id": 31, "name": "vieux.txt", "type": "file", "parent_id": 1, "status": "trashed"},
            ]),
            None,
        ))
        .mount(server)
        .await;
    // /Docs sur deux pages.
    Mock::given(method("GET"))
        .and(path(format!("/3/drive/{DRIVE}/files/10/files")))
        .and(query_param("cursor", "p2"))
        .respond_with(listing(
            json!([{"id": 21, "name": "a.txt", "type": "file", "parent_id": 10, "size": 10}]),
            None,
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/3/drive/{DRIVE}/files/10/files")))
        .respond_with(listing(
            json!([{"id": 20, "name": "budget.xlsx", "type": "file", "parent_id": 10, "size": 1000,
                    "version": {"number": 5, "total_size": 5000, "keep_forever": false}}]),
            Some("p2"),
        ))
        .with_priority(10)
        .mount(server)
        .await;
}

#[tokio::test]
async fn full_then_partial_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    Db::open(&db_path)
        .unwrap()
        .upsert_drives(&[kdvc::api::Drive {
            id: DRIVE,
            name: "d".into(),
            size: None,
            used_size: None,
        }])
        .unwrap();

    // Scan complet.
    let server = MockServer::start().await;
    mount_root(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/3/drive/{DRIVE}/files/11/files")))
        .respond_with(listing(
            json!([{"id": 40, "name": "photo.jpg", "type": "file", "parent_id": 11, "size": 4000}]),
            None,
        ))
        .mount(&server)
        .await;
    assert_eq!(run_scan(&server, &db_path).await, Ok(vec![]));
    let db = Db::open(&db_path).unwrap();
    let s = db.stats(DRIVE).unwrap();
    assert_eq!(
        (s.files, s.dirs, s.versioned_files, s.versions_size),
        (4, 2, 2, 4100)
    );
    assert_eq!(db.entry(DRIVE, 21).unwrap().unwrap().path, "/Docs/a.txt");
    assert!(
        db.entry(DRIVE, 31).unwrap().is_none(),
        "fichier à la corbeille ignoré"
    );

    // Rescan : /Photos en 403 → son contenu est conservé et signalé.
    let server = MockServer::start().await;
    mount_root(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/3/drive/{DRIVE}/files/11/files")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "result": "error", "error": {"code": "forbidden"}
        })))
        .mount(&server)
        .await;
    let failed = run_scan(&server, &db_path).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert!(failed[0].starts_with("/Photos"), "{failed:?}");
    assert!(
        db.entry(DRIVE, 40).unwrap().is_some(),
        "contenu de /Photos conservé"
    );
    assert!(db.drive(DRIVE).unwrap().unwrap().scan_warning.is_some());
}

#[tokio::test]
async fn scan_fails_when_root_or_all_dirs_fail() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let server = MockServer::start().await;
    // Racine introuvable.
    assert!(run_scan(&server, &db_path).await.is_err());

    // Tous les dossiers en échec : erreur, cache inchangé.
    Mock::given(method("GET"))
        .and(path(format!("/3/drive/{DRIVE}/files/1/files")))
        .respond_with(listing(
            json!([{"id": 10, "name": "Docs", "type": "dir", "parent_id": 1}]),
            None,
        ))
        .mount(&server)
        .await;
    let err = run_scan(&server, &db_path).await.unwrap_err();
    assert!(err.contains("aucun dossier"), "{err}");
}
