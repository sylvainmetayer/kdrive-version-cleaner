//! Parcours clavier de la TUI et rendu des écrans, contre une API simulée.
mod common;

use common::*;
use kdvc::api::Drive;
use kdvc::app::{App, Overlay, Tab};
use kdvc::events::AppEvent;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use wiremock::MockServer;

struct Harness {
    app: App,
    rx: UnboundedReceiver<AppEvent>,
    term: Terminal<TestBackend>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new(server: &MockServer, drives: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cache.db");
        let db = seed(&db_path);
        let mut list = vec![Drive {
            id: DRIVE,
            name: "Mon kDrive".into(),
            size: None,
            used_size: None,
        }];
        if drives > 1 {
            list.push(Drive {
                id: 8,
                name: "Autre".into(),
                size: None,
                used_size: None,
            });
        }
        let (tx, rx) = unbounded_channel();
        let app = App::new(client(server), db, db_path, tx, list);
        Self {
            app,
            rx,
            term: Terminal::new(TestBackend::new(140, 40)).unwrap(),
            _dir: dir,
        }
    }

    fn key(&mut self, code: KeyCode) {
        self.key_mod(code, KeyModifiers::NONE);
    }

    fn key_mod(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app
            .on_terminal_event(Event::Key(KeyEvent::new(code, modifiers)))
            .unwrap();
    }

    fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    fn screen(&mut self) -> String {
        self.term
            .draw(|f| kdvc::ui::draw(f, &mut self.app))
            .unwrap();
        let buf = self.term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Transmet les événements des tâches de fond jusqu'à `done`.
    async fn pump(&mut self, done: impl Fn(&AppEvent) -> bool) {
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), self.rx.recv())
                .await
                .expect("délai dépassé")
                .expect("canal fermé");
            let stop = done(&ev);
            self.app.on_app_event(ev).unwrap();
            if stop {
                return;
            }
        }
    }
}

#[tokio::test]
async fn tabs_and_explorer_navigation() {
    let server = MockServer::start().await;
    let mut h = Harness::new(&server, 1);
    h.app.select_drive(DRIVE, false).unwrap();
    assert!(h.app.scan.is_none(), "cache récent : pas de scan");

    let s = h.screen();
    assert!(
        s.contains("Statistiques") && s.contains("Mon kDrive"),
        "{s}"
    );

    h.key(KeyCode::Char('2'));
    assert_eq!(h.app.tab, Tab::Explorer);
    h.key(KeyCode::Enter); // Docs/ (dossiers en premier)
    assert_eq!(h.app.cwd.path, "/Docs");
    let s = h.screen();
    assert!(s.contains("budget.xlsx") && s.contains("a.txt"), "{s}");
    h.key(KeyCode::Left);
    assert_eq!(h.app.cwd.path, "/");

    for (k, tab) in [
        ('3', Tab::Top),
        ('4', Tab::Search),
        ('5', Tab::Log),
        ('1', Tab::Stats),
    ] {
        h.key(KeyCode::Char(k));
        assert_eq!(h.app.tab, tab);
        h.screen();
    }
    h.key(KeyCode::Tab);
    assert_eq!(h.app.tab, Tab::Explorer);
    h.key(KeyCode::BackTab);
    assert_eq!(h.app.tab, Tab::Stats);

    h.key(KeyCode::Char('?'));
    assert!(h.screen().contains("Aide"));
    h.key(KeyCode::Esc);
    assert!(matches!(h.app.overlay, Overlay::None));

    h.key(KeyCode::Char('q'));
    assert!(h.app.quit);
}

#[tokio::test]
async fn manual_deletion_full_flow() {
    let server = MockServer::start().await;
    mock_versions(&server, 20, budget_versions()).await;
    mock_delete(&server, 20, 4, 200).await;
    let mut h = Harness::new(&server, 1);
    h.app.select_drive(DRIVE, false).unwrap();

    // Top fichiers → budget.xlsx (plus gros poids de versions) → versions.
    h.key(KeyCode::Char('3'));
    h.key(KeyCode::Enter);
    h.pump(|e| matches!(e, AppEvent::Versions { .. })).await;
    let s = h.screen();
    assert!(s.contains("protégée (plus récente)"), "{s}");

    // La plus récente est protégée : Espace ne la coche pas.
    h.key(KeyCode::Char(' '));
    let Overlay::Versions(view) = &h.app.overlay else {
        panic!()
    };
    assert!(view.marked.is_empty());
    h.key(KeyCode::Down);
    h.key(KeyCode::Char(' ')); // coche la version 4
    let Overlay::Versions(view) = &h.app.overlay else {
        panic!()
    };
    assert_eq!(view.marked.iter().copied().collect::<Vec<_>>(), vec![4]);

    h.key(KeyCode::Char('c'));
    assert!(matches!(h.app.overlay, Overlay::Preparing { .. }));
    h.screen();
    h.pump(|e| matches!(e, AppEvent::PrepareDone(_))).await;
    let s = h.screen();
    assert!(
        s.contains("Aperçu") && s.contains("1 version(s) à supprimer"),
        "{s}"
    );

    // Confirmation : « non » ne déclenche rien, Échap revient à l'aperçu.
    h.key(KeyCode::Enter);
    h.type_str("non");
    h.key(KeyCode::Enter);
    assert!(matches!(h.app.overlay, Overlay::Confirm { .. }));
    assert!(h.screen().contains("Tapez « oui »"));
    h.key(KeyCode::Esc);
    assert!(matches!(h.app.overlay, Overlay::Preview { .. }));

    h.key(KeyCode::Enter);
    h.type_str("oui");
    h.key(KeyCode::Enter);
    assert!(matches!(h.app.overlay, Overlay::Executing { .. }));
    h.screen();
    h.pump(|e| matches!(e, AppEvent::ExecDone(_))).await;
    let s = h.screen();
    assert!(s.contains("1 version(s) supprimée(s)"), "{s}");
    h.key(KeyCode::Enter);

    h.key(KeyCode::Char('5'));
    let s = h.screen();
    assert!(s.contains("Journal — 1 suppression(s)"), "{s}");
    server.verify().await;
}

#[tokio::test]
async fn rule_picker_and_search() {
    let server = MockServer::start().await;
    mock_versions(&server, 20, budget_versions()).await;
    let mut h = Harness::new(&server, 1);
    h.app.select_drive(DRIVE, false).unwrap();

    // Recherche « bud » puis nettoyage de tous les résultats.
    h.key(KeyCode::Char('4'));
    h.key(KeyCode::Char('/'));
    h.type_str("bud");
    h.key(KeyCode::Tab);
    h.key(KeyCode::BackTab);
    h.key(KeyCode::Enter);
    assert_eq!(h.app.search_results.len(), 1);
    assert!(h.screen().contains("Résultats (1)"));

    h.key(KeyCode::Char('C'));
    assert!(h.screen().contains("Règle de nettoyage"));
    // Paramètre vidé : Entrée est refusé.
    h.key(KeyCode::Backspace);
    h.key(KeyCode::Enter);
    assert!(h.app.status.contains("Paramètre invalide"));
    assert!(matches!(h.app.overlay, Overlay::RulePicker(_)));
    // « plus de 150 jours » : supprime 1 (300 j), protège 2 (keep_forever).
    h.key(KeyCode::Down);
    h.type_str("150");
    h.key(KeyCode::Enter);
    h.pump(|e| matches!(e, AppEvent::PrepareDone(_))).await;
    let Overlay::Preview { plan, .. } = &h.app.overlay else {
        panic!()
    };
    assert_eq!(
        plan.items.iter().map(|i| i.version_id).collect::<Vec<_>>(),
        vec![1]
    );
    h.key(KeyCode::Esc);
    assert!(matches!(h.app.overlay, Overlay::None));

    // Fichier sans ancienne version dans une liste : rien à nettoyer.
    h.key(KeyCode::Char('/'));
    for _ in 0..3 {
        h.key(KeyCode::Backspace);
    }
    h.type_str("zzz");
    h.key(KeyCode::Enter);
    h.key(KeyCode::Char('C'));
    assert!(h.app.status.contains("Aucune ancienne version"));
}

#[tokio::test]
async fn drive_picker_and_scan_events() {
    let server = MockServer::start().await;
    let mut h = Harness::new(&server, 2);
    h.app.open_drive_picker();
    assert!(h.screen().contains("Choisir un drive"));
    h.key(KeyCode::Enter);
    assert_eq!(h.app.drive_id(), Some(DRIVE));

    // Progression puis fin d'un scan partiel.
    h.app
        .on_app_event(AppEvent::ScanProgress {
            files: 1234,
            dirs_done: 1,
            dirs_total: 3,
            current: "Docs".into(),
        })
        .unwrap();
    assert!(h.screen().contains("Scan : 1"));
    h.app
        .on_app_event(AppEvent::ScanDone(Ok(vec!["/Photos (403)".into()])))
        .unwrap();
    assert!(h.app.scan.is_none());
    assert!(h.app.status.contains("Scan partiel"), "{}", h.app.status);
    h.app
        .on_app_event(AppEvent::ScanDone(Err("réseau".into())))
        .unwrap();
    assert!(h.app.status.contains("Échec du scan"));

    // Ctrl+C sans opération en cours : quitte.
    h.key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(h.app.quit);
}
