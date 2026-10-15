mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use common::*;
use kdvc::cleaner::{self, ExecSummary, Plan};
use kdvc::events::AppEvent;
use kdvc::rules::Rule;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use wiremock::MockServer;

async fn wait_prepare(rx: &mut UnboundedReceiver<AppEvent>) -> Result<Plan, String> {
    while let Some(ev) = rx.recv().await {
        if let AppEvent::PrepareDone(res) = ev {
            return res;
        }
    }
    panic!("canal fermé sans PrepareDone");
}

async fn wait_exec(rx: &mut UnboundedReceiver<AppEvent>) -> ExecSummary {
    while let Some(ev) = rx.recv().await {
        if let AppEvent::ExecDone(s) = ev {
            return s;
        }
    }
    panic!("canal fermé sans ExecDone");
}

fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[tokio::test]
async fn prepare_then_execute_keep_last() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let db = seed(&db_path);
    let server = MockServer::start().await;
    mock_versions(&server, 20, budget_versions()).await;
    mock_versions(
        &server,
        30,
        serde_json::json!([
            {"id": 31, "size": 100, "created_at": kdvc::now(), "keep_forever": false},
            {"id": 30, "size": 100, "created_at": kdvc::now() - 50 * DAY, "keep_forever": false},
        ]),
    )
    .await;

    let root = db.entry(DRIVE, 10).unwrap().unwrap();
    let mut targets = db.versioned_descendants(DRIVE, &root).unwrap();
    targets.push(db.entry(DRIVE, 30).unwrap().unwrap());

    let (tx, mut rx) = unbounded_channel();
    cleaner::prepare(
        client(&server),
        db_path.clone(),
        DRIVE,
        targets,
        Rule::KeepLast(2),
        tx.clone(),
        no_cancel(),
    )
    .await;
    let plan = wait_prepare(&mut rx).await.unwrap();

    // budget : garde 5 (courante) et 4, protège 2 (keep_forever) → supprime 3 et 1.
    // notes : 2 versions, garder 2 → rien.
    let ids: Vec<i64> = plan.items.iter().map(|i| i.version_id).collect();
    assert_eq!(ids, vec![3, 1]);
    assert_eq!(plan.total_size, 2000);
    assert_eq!(plan.files_checked, 2);
    assert_eq!(plan.files_affected, 1);
    assert!(plan.errors.is_empty());

    // La 3 est supprimée, la 1 échoue côté serveur (et n'est pas rejouée).
    mock_delete(&server, 20, 3, 200).await;
    mock_delete(&server, 20, 1, 500).await;
    cleaner::execute(
        client(&server),
        db_path.clone(),
        DRIVE,
        plan,
        tx,
        no_cancel(),
    )
    .await;
    let summary = wait_exec(&mut rx).await;
    assert_eq!(
        (summary.deleted, summary.failed, summary.freed),
        (1, 1, 1000)
    );
    assert!(!summary.cancelled);

    let db = kdvc::db::Db::open(&db_path).unwrap();
    let log = db.log(DRIVE).unwrap();
    assert_eq!(log.len(), 2);
    assert!(log.iter().any(|e| e.version_id == 3 && e.ok));
    assert!(
        log.iter()
            .any(|e| e.version_id == 1 && !e.ok && e.error.as_deref().unwrap().contains("boom"))
    );
    // Après un échec, le cache des versions est oublié pour être rechargé.
    assert!(db.cached_versions(DRIVE, 20, 0).unwrap().is_none());
    // … et la référence du scan aussi : la 1 a pu être supprimée côté serveur.
    assert_eq!(db.entry(DRIVE, 20).unwrap().unwrap().scan(), None);
    // Le compte local a perdu la version supprimée.
    assert_eq!(db.entry(DRIVE, 20).unwrap().unwrap().nb_versions, 4);
    server.verify().await;
}

#[tokio::test]
async fn prepare_manual_ignores_protected_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let db = seed(&db_path);
    let server = MockServer::start().await;
    mock_versions(&server, 20, budget_versions()).await;
    // notes.md : pas de mock → 404 → erreur signalée, fichier ignoré.

    let targets = vec![
        db.entry(DRIVE, 20).unwrap().unwrap(),
        db.entry(DRIVE, 30).unwrap().unwrap(),
    ];
    let (tx, mut rx) = unbounded_channel();
    cleaner::prepare(
        client(&server),
        db_path,
        DRIVE,
        targets,
        Rule::Manual([5, 2, 4].into()),
        tx,
        no_cancel(),
    )
    .await;
    let plan = wait_prepare(&mut rx).await.unwrap();
    // 5 = courante, 2 = keep_forever : seule la 4 reste.
    assert_eq!(
        plan.items.iter().map(|i| i.version_id).collect::<Vec<_>>(),
        vec![4]
    );
    assert_eq!(plan.errors.len(), 1);
    assert!(plan.errors[0].contains("/notes.md"), "{:?}", plan.errors);
}

#[tokio::test]
async fn cancelled_operations_stop_early() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let db = seed(&db_path);
    let server = MockServer::start().await;
    let cancel = Arc::new(AtomicBool::new(true));

    let (tx, mut rx) = unbounded_channel();
    let targets = vec![db.entry(DRIVE, 20).unwrap().unwrap()];
    cleaner::prepare(
        client(&server),
        db_path.clone(),
        DRIVE,
        targets,
        Rule::AllButCurrent,
        tx.clone(),
        cancel.clone(),
    )
    .await;
    let plan = wait_prepare(&mut rx).await.unwrap();
    assert!(plan.cancelled);
    assert!(plan.items.is_empty());

    let plan = Plan {
        items: vec![kdvc::cleaner::PlannedDeletion {
            file_id: 20,
            path: "/Docs/budget.xlsx".into(),
            version_id: 1,
            created_at: 0,
            size: 1,
        }],
        ..Default::default()
    };
    cleaner::execute(client(&server), db_path, DRIVE, plan, tx, cancel).await;
    let summary = wait_exec(&mut rx).await;
    assert!(summary.cancelled);
    assert_eq!(summary.deleted + summary.failed, 0);
}

#[tokio::test]
async fn load_versions_uses_cache_then_api() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    let mut db = seed(&db_path);
    let server = MockServer::start().await;
    // a.txt : le scan ne voyait aucune ancienne version, l'API en renvoie 3.
    mock_versions(
        &server,
        21,
        serde_json::json!([
            {"id": 3, "size": 10, "created_at": 3},
            {"id": 2, "size": 7, "created_at": 2},
            {"id": 1, "size": 5, "created_at": 1},
        ]),
    )
    .await;
    let c = client(&server);
    let v = cleaner::load_versions(&c, &mut db, DRIVE, 21)
        .await
        .unwrap();
    assert_eq!(v.len(), 3);
    let e = db.entry(DRIVE, 21).unwrap().unwrap();
    // Liste sans référence de scan : la plus récente est supposée courante.
    assert_eq!((e.nb_versions, e.versions_size), (2, 12));
    // Deuxième appel : servi par le cache (une seule requête au total).
    cleaner::load_versions(&c, &mut db, DRIVE, 21)
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

async fn prepare_plan(
    db_path: &std::path::Path,
    server: &MockServer,
    file_id: i64,
    rule: Rule,
) -> Plan {
    let db = kdvc::db::Db::open(db_path).unwrap();
    let (tx, mut rx) = unbounded_channel();
    cleaner::prepare(
        client(server),
        db_path.to_path_buf(),
        DRIVE,
        vec![db.entry(DRIVE, file_id).unwrap().unwrap()],
        rule,
        tx,
        no_cancel(),
    )
    .await;
    wait_prepare(&mut rx).await.unwrap()
}

#[tokio::test]
async fn list_matching_scan_has_only_old_versions() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    seed(&db_path);
    let server = MockServer::start().await;
    // notes.md : le scan annonce 2 anciennes versions pour 100 octets, la liste
    // concorde. La version courante n'y figure pas : rien n'est protégé d'office.
    let now = kdvc::now();
    mock_versions(
        &server,
        30,
        serde_json::json!([
            {"id": 31, "size": 60, "created_at": now - DAY, "keep_forever": false},
            {"id": 30, "size": 40, "created_at": now - 50 * DAY, "keep_forever": false},
        ]),
    )
    .await;

    // La courante (hors liste) occupe la première des 2 places : seule la 30 part.
    let plan = prepare_plan(&db_path, &server, 30, Rule::KeepLast(2)).await;
    let ids: Vec<i64> = plan.items.iter().map(|i| i.version_id).collect();
    assert_eq!(ids, vec![30]);

    let plan = prepare_plan(&db_path, &server, 30, Rule::AllButCurrent).await;
    let ids: Vec<i64> = plan.items.iter().map(|i| i.version_id).collect();
    assert_eq!(ids, vec![31, 30]);
    assert_eq!(plan.total_size, 100);

    // On supprime d'abord la seule 31 : la référence du scan suit la suppression.
    let mut first = plan.clone();
    first.items.truncate(1);
    mock_delete(&server, 30, 31, 200).await;
    let (tx, mut rx) = unbounded_channel();
    cleaner::execute(
        client(&server),
        db_path.clone(),
        DRIVE,
        first,
        tx,
        no_cancel(),
    )
    .await;
    assert_eq!(wait_exec(&mut rx).await.deleted, 1);
    let e = kdvc::db::Db::open(&db_path)
        .unwrap()
        .entry(DRIVE, 30)
        .unwrap()
        .unwrap();
    assert_eq!((e.nb_versions, e.versions_size), (1, 40));
    assert_eq!((e.scan_versions, e.scan_versions_size), (Some(1), Some(40)));

    // Liste en cache (sans la 31) toujours conforme : la 30 reste supprimable.
    let plan = prepare_plan(&db_path, &server, 30, Rule::AllButCurrent).await;
    let ids: Vec<i64> = plan.items.iter().map(|i| i.version_id).collect();
    assert_eq!(ids, vec![30]);
    // Une seule requête GET /versions : le reste vient du cache.
    let gets = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "GET")
        .count();
    assert_eq!(gets, 1);
    server.verify().await;
}

#[tokio::test]
async fn list_not_matching_scan_protects_newest() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    seed(&db_path);
    let server = MockServer::start().await;
    // Une version de plus que le scan : la plus récente peut être la courante.
    let now = kdvc::now();
    mock_versions(
        &server,
        30,
        serde_json::json!([
            {"id": 32, "size": 100, "created_at": now, "keep_forever": false},
            {"id": 31, "size": 60, "created_at": now - DAY, "keep_forever": false},
            {"id": 30, "size": 40, "created_at": now - 50 * DAY, "keep_forever": false},
        ]),
    )
    .await;
    let plan = prepare_plan(&db_path, &server, 30, Rule::Manual([32, 31].into())).await;
    let ids: Vec<i64> = plan.items.iter().map(|i| i.version_id).collect();
    assert_eq!(ids, vec![31]);
}
