//! Données et serveur simulés partagés par les tests d'intégration.
#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use kdvc::api::models::FileVersionInfo;
use kdvc::api::{Client, Drive, RemoteFile};
use kdvc::db::Db;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const DRIVE: i64 = 7;
pub const DAY: i64 = 86_400;

pub fn client(server: &MockServer) -> Client {
    Client::with_options("tok".into(), server.uri(), Duration::ZERO).unwrap()
}

fn file(id: i64, parent: i64, name: &str, size: i64, versions: Option<(i64, i64)>) -> RemoteFile {
    RemoteFile {
        id,
        name: name.into(),
        kind: Some(
            if versions.is_none() && size == 0 {
                "dir"
            } else {
                "file"
            }
            .into(),
        ),
        parent_id: Some(parent),
        size: Some(size),
        last_modified_at: Some(kdvc::now() - DAY),
        status: None,
        version: versions.map(|(number, total_size)| FileVersionInfo {
            number,
            total_size: Some(total_size),
            keep_forever: false,
        }),
    }
}

/// Arborescence : /Docs/budget.xlsx (5 versions), /Docs/a.txt (1 version), /notes.md (2 versions).
pub fn seed(db_path: &Path) -> Db {
    let mut db = Db::open(db_path).unwrap();
    db.upsert_drives(&[Drive {
        id: DRIVE,
        name: "Mon kDrive".into(),
        size: Some(1 << 40),
        used_size: Some(50_000_000),
    }])
    .unwrap();
    let files = vec![
        file(10, 1, "Docs", 0, None),
        file(20, 10, "budget.xlsx", 1000, Some((5, 5000))),
        file(21, 10, "a.txt", 10, Some((1, 10))),
        file(30, 1, "notes.md", 100, Some((2, 200))),
    ];
    db.replace_tree(DRIVE, &files, &[], None, kdvc::now())
        .unwrap();
    db
}

/// Versions de budget.xlsx : 5 (courante) … 1 (la plus ancienne), la 2 « conservée pour toujours ».
pub fn budget_versions() -> serde_json::Value {
    let now = kdvc::now();
    json!([
        {"id": 5, "size": 1000, "created_at": now - DAY, "keep_forever": false},
        {"id": 4, "size": 1000, "created_at": now - 10 * DAY, "keep_forever": false},
        {"id": 3, "size": 1000, "created_at": now - 100 * DAY, "keep_forever": false},
        {"id": 2, "size": 1000, "created_at": now - 200 * DAY, "keep_forever": true},
        {"id": 1, "size": 1000, "created_at": now - 300 * DAY, "keep_forever": false},
    ])
}

pub async fn mock_versions(server: &MockServer, file_id: i64, data: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!("/3/drive/{DRIVE}/files/{file_id}/versions")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": "success", "data": data, "page": 1, "pages": 1
        })))
        .mount(server)
        .await;
}

pub async fn mock_delete(server: &MockServer, file_id: i64, version_id: i64, status: u16) {
    let body = if status == 200 {
        json!({"result": "success", "data": true})
    } else {
        json!({"result": "error", "error": {"code": "boom", "description": "échec simulé"}})
    };
    Mock::given(method("DELETE"))
        .and(path(format!(
            "/2/drive/{DRIVE}/files/{file_id}/versions/{version_id}"
        )))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}
