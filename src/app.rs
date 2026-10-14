use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{Client, Drive, ROOT_DIR_ID, Version};
use crate::cleaner::{self, Cancel, ExecSummary, Plan};
use crate::db::{Db, DriveInfo, Entry, LogEntry, SearchFilter, Stats};
use crate::events::AppEvent;
use crate::now;
use crate::rules::{self, Rule};

/// Au-delà, on suggère un rescan.
const STALE_AFTER_SECS: i64 = 7 * 86_400;
const LIST_LIMIT: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Stats,
    Explorer,
    Top,
    Search,
    Log,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Stats, Tab::Explorer, Tab::Top, Tab::Search, Tab::Log];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Stats => "Stats",
            Tab::Explorer => "Explorateur",
            Tab::Top => "Top fichiers",
            Tab::Search => "Recherche",
            Tab::Log => "Journal",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

pub struct ScanState {
    pub files: usize,
    pub dirs_done: usize,
    pub dirs_total: usize,
    pub current: String,
    pub started: Instant,
}

pub struct VersionsView {
    pub entry: Entry,
    /// `None` pendant le chargement.
    pub versions: Option<Vec<Version>>,
    pub error: Option<String>,
    pub current_id: Option<i64>,
    pub marked: HashSet<i64>,
    pub state: TableState,
}

pub const RULE_CHOICES: usize = 3;

pub struct RulePicker {
    pub label: String,
    pub targets: Vec<Entry>,
    pub selected: usize,
    pub keep_n: String,
    pub days: String,
    /// Le premier chiffre tapé remplace la valeur par défaut.
    pub typed: bool,
}

impl RulePicker {
    /// `None` si le paramètre de la règle est vide ou invalide (dont « garder 0 ») :
    /// on ne retombe jamais sur la valeur la plus destructrice.
    pub fn rule(&self, index: usize) -> Option<Rule> {
        Some(match index {
            0 => Rule::KeepLast(self.keep_n.parse().ok().filter(|n| *n >= 1)?),
            1 => Rule::OlderThan {
                days: self.days.parse().ok()?,
            },
            _ => Rule::AllButCurrent,
        })
    }

    fn param(&mut self) -> Option<&mut String> {
        match self.selected {
            0 => Some(&mut self.keep_n),
            1 => Some(&mut self.days),
            _ => None,
        }
    }
}

/// Suite à donner à une touche dans le sélecteur de règle.
#[derive(Debug, PartialEq, Eq)]
pub enum PickerAction {
    None,
    Close,
    Invalid,
    Apply(Rule),
}

impl RulePicker {
    pub fn on_key(&mut self, code: KeyCode) -> PickerAction {
        match code {
            KeyCode::Esc => return PickerAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.typed = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(RULE_CHOICES - 1);
                self.typed = false;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('-' | '+') => {
                let step: i64 = if self.selected == 1 { 30 } else { 1 };
                let down = matches!(code, KeyCode::Left | KeyCode::Char('-'));
                // On garde au moins la version courante.
                let min = if self.selected == 0 { 1 } else { 0 };
                if let Some(param) = self.param() {
                    let v = param.parse::<i64>().unwrap_or(0) + if down { -step } else { step };
                    *param = v.clamp(min, 99_999).to_string();
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let first = !std::mem::replace(&mut self.typed, true);
                if let Some(param) = self.param() {
                    if first || param == "0" {
                        param.clear();
                    }
                    if param.len() < 5 {
                        param.push(c);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(param) = self.param() {
                    param.pop();
                }
            }
            KeyCode::Enter => {
                return match self.rule(self.selected) {
                    Some(rule) => PickerAction::Apply(rule),
                    None => PickerAction::Invalid,
                };
            }
            _ => {}
        }
        PickerAction::None
    }
}

impl VersionsView {
    /// Coche ou décoche la version sélectionnée, sauf si elle est protégée.
    pub fn toggle_selected(&mut self) {
        let len = self.versions.as_ref().map_or(0, Vec::len);
        let selected = self
            .state
            .selected()
            .and_then(|i| self.versions.as_ref()?.get(i));
        if let Some(v) = selected
            && Some(v.id) != self.current_id
            && !v.keep_forever
        {
            let id = v.id;
            if !self.marked.remove(&id) {
                self.marked.insert(id);
            }
            navigate(&mut self.state, len, KeyCode::Down);
        }
    }
}

pub enum Overlay {
    None,
    DrivePicker(TableState),
    Versions(VersionsView),
    RulePicker(RulePicker),
    Preparing {
        label: String,
        done: usize,
        total: usize,
        started: Instant,
    },
    Preview {
        plan: Plan,
        state: TableState,
    },
    Confirm {
        plan: Plan,
        input: String,
    },
    Executing {
        done: usize,
        total: usize,
        freed: i64,
        failed: usize,
    },
    Summary(ExecSummary),
    Help,
}

pub struct SearchForm {
    pub fields: [String; 4],
    pub focus: usize,
    pub editing: bool,
    /// Filtre de la dernière recherche lancée, réutilisé au rafraîchissement
    /// (les champs peuvent être en cours de modification). `None` avant la première recherche.
    pub applied: Option<SearchFilter>,
}

impl SearchForm {
    pub const LABELS: [&str; 4] = ["Nom", "Extension", "Taille min. (Mo)", "Versions min."];

    fn filter(&self) -> SearchFilter {
        SearchFilter {
            text: self.fields[0].clone(),
            extension: self.fields[1].clone(),
            min_size: (self.fields[2]
                .trim()
                .replace(',', ".")
                .parse::<f64>()
                .unwrap_or(0.0)
                * 1024.0
                * 1024.0) as i64,
            min_versions: self.fields[3].trim().parse().unwrap_or(2),
        }
    }
}

pub struct App {
    pub client: Client,
    pub db: Db,
    pub db_path: PathBuf,
    pub tx: UnboundedSender<AppEvent>,

    pub drives: Vec<Drive>,
    pub drive: Option<DriveInfo>,
    pub stats: Stats,
    pub tab: Tab,

    pub cwd: Entry,
    pub explorer: Vec<Entry>,
    pub explorer_state: TableState,
    pub top: Vec<Entry>,
    pub top_state: TableState,
    pub search: SearchForm,
    pub search_results: Vec<Entry>,
    pub search_state: TableState,
    pub log: Vec<LogEntry>,
    pub log_state: TableState,

    pub scan: Option<ScanState>,
    pub overlay: Overlay,
    pub status: String,
    pub cancel: Option<Cancel>,
    /// `--rescan` demandé alors que le drive sera choisi dans le sélecteur.
    pub rescan_on_select: bool,
    pub quit: bool,
}

fn root_entry() -> Entry {
    Entry {
        id: ROOT_DIR_ID,
        parent_id: 0,
        name: String::new(),
        path: "/".into(),
        is_dir: true,
        size: 0,
        nb_versions: 0,
        versions_size: 0,
        versioned_files: 0,
        last_modified_at: 0,
    }
}

/// Ajuste la sélection d'une table après rechargement de ses lignes.
fn clamp(state: &mut TableState, len: usize) {
    match (state.selected(), len) {
        (_, 0) => state.select(None),
        (None, _) => state.select(Some(0)),
        (Some(i), n) if i >= n => state.select(Some(n - 1)),
        _ => {}
    }
}

fn navigate(state: &mut TableState, len: usize, key: KeyCode) -> bool {
    if len == 0 {
        return matches!(key, KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k'));
    }
    let i = state.selected().unwrap_or(0);
    let next = match key {
        KeyCode::Up | KeyCode::Char('k') => i.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => (i + 1).min(len - 1),
        KeyCode::PageUp => i.saturating_sub(15),
        KeyCode::PageDown => (i + 15).min(len - 1),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => len - 1,
        _ => return false,
    };
    state.select(Some(next));
    true
}

impl App {
    pub fn new(
        client: Client,
        db: Db,
        db_path: PathBuf,
        tx: UnboundedSender<AppEvent>,
        drives: Vec<Drive>,
    ) -> Self {
        Self {
            client,
            db,
            db_path,
            tx,
            drives,
            drive: None,
            stats: Stats::default(),
            tab: Tab::Stats,
            cwd: root_entry(),
            explorer: Vec::new(),
            explorer_state: TableState::default(),
            top: Vec::new(),
            top_state: TableState::default(),
            search: SearchForm {
                fields: [String::new(), String::new(), String::new(), "2".into()],
                focus: 0,
                editing: false,
                applied: None,
            },
            search_results: Vec::new(),
            search_state: TableState::default(),
            log: Vec::new(),
            log_state: TableState::default(),
            scan: None,
            overlay: Overlay::None,
            status: String::new(),
            cancel: None,
            rescan_on_select: false,
            quit: false,
        }
    }

    pub fn drive_id(&self) -> Option<i64> {
        self.drive.as_ref().map(|d| d.id)
    }

    pub fn open_drive_picker(&mut self) {
        let mut state = TableState::default();
        let current = self
            .drive_id()
            .and_then(|id| self.drives.iter().position(|d| d.id == id));
        state.select(Some(current.unwrap_or(0)));
        self.overlay = Overlay::DrivePicker(state);
    }

    pub fn select_drive(&mut self, drive_id: i64, force_rescan: bool) -> Result<()> {
        self.drive = self.db.drive(drive_id)?;
        self.cwd = root_entry();
        self.explorer_state = TableState::default();
        self.top_state = TableState::default();
        self.search_results.clear();
        self.refresh()?;
        let scanned_at = self.drive.as_ref().and_then(|d| d.scanned_at);
        match scanned_at {
            _ if force_rescan => self.start_scan(),
            None => self.start_scan(),
            Some(ts) if now() - ts > STALE_AFTER_SECS => {
                self.status = format!(
                    "Cache du {} : appuyez sur r pour rescanner.",
                    crate::ui::fmt_date(ts)
                );
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Recharge toutes les vues depuis le cache.
    pub fn refresh(&mut self) -> Result<()> {
        let Some(id) = self.drive_id() else {
            return Ok(());
        };
        self.drive = self.db.drive(id)?;
        self.stats = self.db.stats(id)?;
        if self.cwd.id != ROOT_DIR_ID {
            self.cwd = self.db.entry(id, self.cwd.id)?.unwrap_or_else(root_entry);
        }
        self.explorer = self.db.children(id, self.cwd.id)?;
        clamp(&mut self.explorer_state, self.explorer.len());
        self.top = self.db.top(id, LIST_LIMIT)?;
        clamp(&mut self.top_state, self.top.len());
        // Relance aussi une recherche restée vide : un scan peut lui donner des résultats.
        if let Some(filter) = &self.search.applied {
            self.search_results = self.db.search(id, filter, LIST_LIMIT)?;
            clamp(&mut self.search_state, self.search_results.len());
        }
        self.log = self.db.log(id)?;
        clamp(&mut self.log_state, self.log.len());
        Ok(())
    }

    pub fn start_scan(&mut self) {
        let Some(id) = self.drive_id() else { return };
        if self.scan.is_some() || self.busy() {
            return;
        }
        self.scan = Some(ScanState {
            files: 0,
            dirs_done: 0,
            dirs_total: 0,
            current: String::new(),
            started: Instant::now(),
        });
        self.status.clear();
        tokio::spawn(crate::scanner::scan(
            self.client.clone(),
            self.db_path.clone(),
            id,
            self.tx.clone(),
        ));
    }

    /// Une préparation ou une suppression est en cours.
    fn busy(&self) -> bool {
        matches!(
            self.overlay,
            Overlay::Preparing { .. } | Overlay::Executing { .. }
        )
    }

    /// Une opération longue est en cours : l'écran est redessiné plus souvent
    /// pour animer l'indicateur de chargement.
    pub fn loading(&self) -> bool {
        self.busy()
            || self.scan.is_some()
            || matches!(&self.overlay, Overlay::Versions(v) if v.versions.is_none() && v.error.is_none())
    }

    // ---------- événements de fond ----------

    pub fn on_app_event(&mut self, ev: AppEvent) -> Result<()> {
        match ev {
            AppEvent::ScanProgress {
                files,
                dirs_done,
                dirs_total,
                current,
            } => {
                let started = self.scan.as_ref().map_or_else(Instant::now, |s| s.started);
                self.scan = Some(ScanState {
                    files,
                    dirs_done,
                    dirs_total,
                    current,
                    started,
                });
            }
            AppEvent::ScanDone(res) => {
                self.scan = None;
                match res {
                    Ok(failed) => {
                        self.status = match self.refresh() {
                            Ok(()) if !failed.is_empty() => format!(
                                "Scan partiel : {} dossier(s) non rescanné(s) : {}",
                                failed.len(),
                                failed.join(", ")
                            ),
                            Ok(()) => format!(
                                "Scan terminé : {} fichiers, {} versionnés.",
                                crate::ui::fmt_count(self.stats.files),
                                crate::ui::fmt_count(self.stats.versioned_files)
                            ),
                            Err(e) => format!("Scan terminé, lecture du cache impossible : {e:#}"),
                        };
                    }
                    Err(e) => self.status = format!("Échec du scan : {e}"),
                }
            }
            AppEvent::Versions { file_id, result } => {
                let mut cache_changed = false;
                if let Overlay::Versions(view) = &mut self.overlay
                    && view.entry.id == file_id
                {
                    match result {
                        Ok(mut versions) => {
                            rules::sort_newest_first(&mut versions);
                            view.current_id = rules::current_version_id(&versions);
                            // Le chargement a pu réaligner le compte et le poids du fichier.
                            let drive_id = self.drive.as_ref().map(|d| d.id);
                            // Une erreur de lecture ne doit pas empêcher l'affichage des versions.
                            if let Some(id) = drive_id
                                && let Ok(Some(entry)) = self.db.entry(id, file_id)
                            {
                                cache_changed = entry.nb_versions != view.entry.nb_versions
                                    || entry.versions_size != view.entry.versions_size;
                                view.entry = entry;
                            }
                            clamp(&mut view.state, versions.len());
                            view.versions = Some(versions);
                        }
                        Err(e) => view.error = Some(e),
                    }
                }
                if cache_changed {
                    self.refresh()?;
                }
            }
            AppEvent::PrepareProgress { done, total } => {
                if let Overlay::Preparing {
                    done: d, total: t, ..
                } = &mut self.overlay
                {
                    *d = done;
                    *t = total;
                }
            }
            AppEvent::PrepareDone(res) => {
                self.cancel = None;
                match res {
                    Ok(plan) if plan.cancelled => {
                        self.overlay = Overlay::None;
                        self.status = "Aperçu annulé.".into();
                    }
                    Ok(plan) => {
                        let mut state = TableState::default();
                        clamp(&mut state, plan.items.len());
                        self.overlay = Overlay::Preview { plan, state };
                    }
                    Err(e) => {
                        self.overlay = Overlay::None;
                        self.status = format!("Échec de l'aperçu : {e}");
                    }
                }
            }
            AppEvent::ExecProgress {
                done,
                total,
                freed,
                failed,
            } => {
                self.overlay = Overlay::Executing {
                    done,
                    total,
                    freed,
                    failed,
                };
            }
            AppEvent::ExecDone(summary) => {
                self.cancel = None;
                // Le bilan doit s'afficher même si la relecture du cache échoue.
                self.overlay = Overlay::Summary(summary);
                if let Err(e) = self.refresh() {
                    self.status = format!("Lecture du cache impossible : {e:#}");
                }
            }
        }
        Ok(())
    }

    // ---------- clavier ----------

    pub fn on_terminal_event(&mut self, ev: Event) -> Result<()> {
        let Event::Key(key) = ev else { return Ok(()) };
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Premier Ctrl+C : annulation propre ; second : on quitte même si une
            // requête reste bloquée (attente de réessai, timeout…).
            match &self.cancel {
                Some(c) if !c.swap(true, Ordering::Relaxed) => {
                    self.status = "Annulation… (Ctrl+C à nouveau pour quitter)".into();
                }
                _ => self.quit = true,
            }
            return Ok(());
        }
        if !matches!(self.overlay, Overlay::None) {
            return self.on_overlay_key(key);
        }
        if self.tab == Tab::Search && self.search.editing {
            return self.on_search_edit_key(key);
        }
        self.on_main_key(key)
    }

    fn on_main_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.overlay = Overlay::Help,
            KeyCode::Char(c @ '1'..='5') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
            }
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            KeyCode::BackTab => {
                self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
            }
            KeyCode::Char('r') => self.start_scan(),
            KeyCode::Char('d') if self.drives.len() > 1 && self.scan.is_none() => {
                self.open_drive_picker()
            }
            KeyCode::Char('c') => self.cleanup_selected()?,
            KeyCode::Char('C') => self.cleanup_all()?,
            _ => self.on_tab_key(key)?,
        }
        Ok(())
    }

    fn on_tab_key(&mut self, key: KeyEvent) -> Result<()> {
        match self.tab {
            Tab::Stats => {}
            Tab::Explorer => {
                if navigate(&mut self.explorer_state, self.explorer.len(), key.code) {
                    return Ok(());
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(e) = self.selected_entry() {
                            if e.is_dir {
                                self.enter_dir(e)?;
                            } else {
                                self.open_versions(e);
                            }
                        }
                    }
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.leave_dir()?,
                    _ => {}
                }
            }
            Tab::Top | Tab::Search => {
                let (state, len) = if self.tab == Tab::Top {
                    (&mut self.top_state, self.top.len())
                } else {
                    (&mut self.search_state, self.search_results.len())
                };
                if navigate(state, len, key.code) {
                    return Ok(());
                }
                match key.code {
                    KeyCode::Enter => {
                        if let Some(e) = self.selected_entry() {
                            self.open_versions(e);
                        }
                    }
                    KeyCode::Char('/') if self.tab == Tab::Search => self.search.editing = true,
                    _ => {}
                }
            }
            Tab::Log => {
                if navigate(&mut self.log_state, self.log.len(), key.code) {
                    return Ok(());
                }
                if key.code == KeyCode::Char('e') {
                    self.status = match self.export_log(Path::new(".")) {
                        Ok(path) => format!("Journal exporté dans {}", path.display()),
                        Err(e) => format!("Export impossible : {e:#}"),
                    };
                }
            }
        }
        Ok(())
    }

    fn on_search_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        let f = &mut self.search;
        match key.code {
            KeyCode::Esc => f.editing = false,
            KeyCode::Tab | KeyCode::Down => f.focus = (f.focus + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => f.focus = (f.focus + 3) % 4,
            KeyCode::Backspace => {
                f.fields[f.focus].pop();
            }
            KeyCode::Char(c) => {
                // Les champs numériques n'acceptent que des chiffres.
                if f.focus < 2 || c.is_ascii_digit() || (f.focus == 2 && matches!(c, '.' | ',')) {
                    f.fields[f.focus].push(c);
                }
            }
            KeyCode::Enter => {
                f.editing = false;
                if let Some(id) = self.drive_id() {
                    let filter = self.search.filter();
                    self.search_results = self.db.search(id, &filter, LIST_LIMIT)?;
                    self.search.applied = Some(filter);
                    self.search_state = TableState::default();
                    clamp(&mut self.search_state, self.search_results.len());
                    self.status = format!("{} résultat(s).", self.search_results.len());
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        let code = key.code;
        match self.overlay {
            Overlay::None => Ok(()),
            Overlay::Help | Overlay::Summary(_) => {
                if matches!(
                    code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?')
                ) {
                    self.overlay = Overlay::None;
                }
                Ok(())
            }
            Overlay::DrivePicker(_) => self.on_drive_picker_key(code),
            Overlay::Versions(_) => {
                self.on_versions_key(code);
                Ok(())
            }
            Overlay::RulePicker(_) => {
                self.on_rule_picker_key(code);
                Ok(())
            }
            Overlay::Preparing { .. } | Overlay::Executing { .. } => {
                if code == KeyCode::Esc
                    && let Some(c) = &self.cancel
                {
                    c.store(true, Ordering::Relaxed);
                    self.status = "Annulation…".into();
                }
                Ok(())
            }
            Overlay::Preview { .. } => {
                self.on_preview_key(code);
                Ok(())
            }
            Overlay::Confirm { .. } => {
                self.on_confirm_key(code);
                Ok(())
            }
        }
    }

    fn on_drive_picker_key(&mut self, code: KeyCode) -> Result<()> {
        let Overlay::DrivePicker(state) = &mut self.overlay else {
            return Ok(());
        };
        if navigate(state, self.drives.len(), code) {
            return Ok(());
        }
        match code {
            KeyCode::Enter => {
                if let Some(d) = state.selected().and_then(|i| self.drives.get(i)) {
                    let id = d.id;
                    self.overlay = Overlay::None;
                    let rescan = std::mem::take(&mut self.rescan_on_select);
                    self.select_drive(id, rescan)?;
                }
            }
            KeyCode::Esc if self.drive.is_some() => self.overlay = Overlay::None,
            KeyCode::Char('q') if self.drive.is_none() => self.quit = true,
            _ => {}
        }
        Ok(())
    }

    fn on_versions_key(&mut self, code: KeyCode) {
        let Overlay::Versions(view) = &mut self.overlay else {
            return;
        };
        let len = view.versions.as_ref().map_or(0, Vec::len);
        if navigate(&mut view.state, len, code) {
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Char(' ') => view.toggle_selected(),
            KeyCode::Char('c') if view.versions.is_some() => {
                let entry = view.entry.clone();
                if view.marked.is_empty() {
                    self.open_rule_picker(entry.path.clone(), vec![entry]);
                } else {
                    let rule = Rule::Manual(view.marked.clone());
                    self.start_prepare(entry.path.clone(), vec![entry], rule);
                }
            }
            _ => {}
        }
    }

    fn on_rule_picker_key(&mut self, code: KeyCode) {
        let Overlay::RulePicker(p) = &mut self.overlay else {
            return;
        };
        match p.on_key(code) {
            PickerAction::None => {}
            PickerAction::Close => self.overlay = Overlay::None,
            PickerAction::Invalid => {
                self.status =
                    "Paramètre invalide : saisissez un nombre (au moins 1 version à garder).".into()
            }
            PickerAction::Apply(rule) => {
                let label = std::mem::take(&mut p.label);
                let targets = std::mem::take(&mut p.targets);
                self.start_prepare(label, targets, rule);
            }
        }
    }

    fn on_preview_key(&mut self, code: KeyCode) {
        let Overlay::Preview { plan, state } = &mut self.overlay else {
            return;
        };
        if navigate(state, plan.items.len(), code) {
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Enter | KeyCode::Char('y') if !plan.items.is_empty() => {
                let plan = std::mem::take(plan);
                self.overlay = Overlay::Confirm {
                    plan,
                    input: String::new(),
                };
            }
            _ => {}
        }
    }

    fn on_confirm_key(&mut self, code: KeyCode) {
        let Overlay::Confirm { plan, input } = &mut self.overlay else {
            return;
        };
        match code {
            KeyCode::Esc => {
                let plan = std::mem::take(plan);
                let mut state = TableState::default();
                clamp(&mut state, plan.items.len());
                self.overlay = Overlay::Preview { plan, state };
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if input.len() < 10 => input.push(c),
            KeyCode::Enter if input.trim().eq_ignore_ascii_case("oui") => {
                let plan = std::mem::take(plan);
                self.start_execute(plan);
            }
            _ => {}
        }
    }

    // ---------- actions ----------

    fn selected_entry(&self) -> Option<Entry> {
        let (list, state) = match self.tab {
            Tab::Explorer => (&self.explorer, &self.explorer_state),
            Tab::Top => (&self.top, &self.top_state),
            Tab::Search => (&self.search_results, &self.search_state),
            _ => return None,
        };
        state.selected().and_then(|i| list.get(i)).cloned()
    }

    fn enter_dir(&mut self, dir: Entry) -> Result<()> {
        let Some(id) = self.drive_id() else {
            return Ok(());
        };
        self.explorer = self.db.children(id, dir.id)?;
        self.cwd = dir;
        self.explorer_state = TableState::default();
        clamp(&mut self.explorer_state, self.explorer.len());
        Ok(())
    }

    fn leave_dir(&mut self) -> Result<()> {
        let Some(id) = self.drive_id() else {
            return Ok(());
        };
        if self.cwd.id == ROOT_DIR_ID {
            return Ok(());
        }
        let child = self.cwd.id;
        let parent = self
            .db
            .entry(id, self.cwd.parent_id)?
            .unwrap_or_else(root_entry);
        self.enter_dir(parent)?;
        // Replace le curseur sur le dossier d'où l'on vient.
        if let Some(i) = self.explorer.iter().position(|e| e.id == child) {
            self.explorer_state.select(Some(i));
        }
        Ok(())
    }

    fn open_versions(&mut self, entry: Entry) {
        let Some(drive_id) = self.drive_id() else {
            return;
        };
        let file_id = entry.id;
        self.overlay = Overlay::Versions(VersionsView {
            entry,
            versions: None,
            error: None,
            current_id: None,
            marked: HashSet::new(),
            state: TableState::default(),
        });
        let client = self.client.clone();
        let db_path = self.db_path.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut db = Db::open(&db_path)?;
                cleaner::load_versions(&client, &mut db, drive_id, file_id).await
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Versions { file_id, result });
        });
    }

    fn cleanup_selected(&mut self) -> Result<()> {
        let Some(entry) = self.selected_entry() else {
            return Ok(());
        };
        let targets = self.targets_for(&entry)?;
        self.open_rule_picker(entry.path.clone(), targets);
        Ok(())
    }

    fn cleanup_all(&mut self) -> Result<()> {
        let Some(id) = self.drive_id() else {
            return Ok(());
        };
        let (label, targets) = match self.tab {
            Tab::Explorer => (
                format!("{} (tout le dossier)", self.cwd.path),
                self.db.versioned_descendants(id, &self.cwd)?,
            ),
            Tab::Top => (
                format!("les {} fichiers du top", self.top.len()),
                self.top.clone(),
            ),
            Tab::Search => (
                format!("les {} résultats de recherche", self.search_results.len()),
                // « Versions min. » peut être inférieur à 2 : on évite une requête
                // par fichier sans ancienne version connue.
                self.search_results
                    .iter()
                    .filter(|e| e.nb_versions > 1)
                    .cloned()
                    .collect(),
            ),
            _ => return Ok(()),
        };
        self.open_rule_picker(label, targets);
        Ok(())
    }

    fn targets_for(&self, entry: &Entry) -> Result<Vec<Entry>> {
        let Some(id) = self.drive_id() else {
            return Ok(vec![]);
        };
        if entry.is_dir {
            self.db.versioned_descendants(id, entry)
        } else {
            Ok(vec![entry.clone()])
        }
    }

    /// Un scan remplace le cache : on ne prépare aucun nettoyage pendant ce temps.
    fn scan_blocks_cleanup(&mut self) -> bool {
        if self.scan.is_some() {
            self.status = "Attendez la fin du scan avant de nettoyer.".into();
        }
        self.scan.is_some()
    }

    fn open_rule_picker(&mut self, label: String, targets: Vec<Entry>) {
        if self.scan_blocks_cleanup() {
            return;
        }
        // Les listes arrivent déjà filtrées sur le compte du scan ; un fichier choisi
        // seul ne l'est pas : l'aperçu chargera ses versions, ce compte pouvant être périmé.
        let targets: Vec<Entry> = targets.into_iter().filter(|e| !e.is_dir).collect();
        if targets.is_empty() {
            self.overlay = Overlay::None;
            self.status = "Aucune ancienne version dans la sélection.".into();
            return;
        }
        self.overlay = Overlay::RulePicker(RulePicker {
            label,
            targets,
            selected: 0,
            keep_n: "1".into(),
            days: "90".into(),
            typed: false,
        });
    }

    fn start_prepare(&mut self, label: String, targets: Vec<Entry>, rule: Rule) {
        let Some(drive_id) = self.drive_id() else {
            return;
        };
        // La sélection manuelle passe directement ici, sans le sélecteur de règle.
        if self.scan_blocks_cleanup() {
            return;
        }
        let cancel: Cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.overlay = Overlay::Preparing {
            label,
            done: 0,
            total: targets.len(),
            started: Instant::now(),
        };
        tokio::spawn(cleaner::prepare(
            self.client.clone(),
            self.db_path.clone(),
            drive_id,
            targets,
            rule,
            self.tx.clone(),
            cancel,
        ));
    }

    fn start_execute(&mut self, plan: Plan) {
        let Some(drive_id) = self.drive_id() else {
            return;
        };
        let cancel: Cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.overlay = Overlay::Executing {
            done: 0,
            total: plan.items.len(),
            freed: 0,
            failed: 0,
        };
        tokio::spawn(cleaner::execute(
            self.client.clone(),
            self.db_path.clone(),
            drive_id,
            plan,
            self.tx.clone(),
            cancel,
        ));
    }

    /// Exporte le journal en CSV dans `dir` et renvoie le chemin du fichier.
    pub fn export_log(&self, dir: &Path) -> Result<PathBuf> {
        let path = dir.join(format!(
            "kdvc-journal-{}.csv",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        ));
        {
            let mut w = csv::Writer::from_path(&path)?;
            w.write_record([
                "date",
                "drive_id",
                "file_id",
                "chemin",
                "version_id",
                "date_version",
                "taille_octets",
                "statut",
                "erreur",
            ])?;
            for e in &self.log {
                w.write_record([
                    crate::ui::fmt_date(e.ts),
                    e.drive_id.to_string(),
                    e.file_id.to_string(),
                    e.path.clone(),
                    e.version_id.to_string(),
                    crate::ui::fmt_date(e.version_created_at),
                    e.size.to_string(),
                    if e.ok { "supprimée" } else { "échec" }.to_string(),
                    e.error.clone().unwrap_or_default(),
                ])?;
            }
            w.flush()?;
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(keep_n: &str, days: &str) -> RulePicker {
        RulePicker {
            label: String::new(),
            targets: Vec::new(),
            selected: 0,
            keep_n: keep_n.into(),
            days: days.into(),
            typed: false,
        }
    }

    #[test]
    fn rule_picker_rejects_empty_or_zero_keep() {
        assert_eq!(picker("3", "90").rule(0), Some(Rule::KeepLast(3)));
        assert_eq!(picker("", "90").rule(0), None);
        assert_eq!(picker("0", "90").rule(0), None);
        assert_eq!(picker("1", "").rule(1), None);
        assert_eq!(picker("1", "0").rule(1), Some(Rule::OlderThan { days: 0 }));
        assert_eq!(picker("", "").rule(2), Some(Rule::AllButCurrent));
    }

    #[test]
    fn rule_picker_keys() {
        let mut p = picker("1", "90");
        assert_eq!(p.on_key(KeyCode::Esc), PickerAction::Close);
        // « garder N » ne descend pas sous 1.
        p.on_key(KeyCode::Left);
        assert_eq!(p.keep_n, "1");
        p.on_key(KeyCode::Char('+'));
        p.on_key(KeyCode::Right);
        assert_eq!(p.keep_n, "3");
        // Jours : pas de 30, plancher à 0.
        p.on_key(KeyCode::Char('j'));
        p.on_key(KeyCode::Char('-'));
        assert_eq!(p.days, "60");
        for _ in 0..5 {
            p.on_key(KeyCode::Left);
        }
        assert_eq!(p.days, "0");
        // Le premier chiffre tapé remplace la valeur, 5 chiffres au plus.
        p.on_key(KeyCode::Char('k'));
        p.on_key(KeyCode::Up);
        assert_eq!(p.selected, 0);
        for c in "1234567".chars() {
            p.on_key(KeyCode::Char(c));
        }
        assert_eq!(p.keep_n, "12345");
        assert_eq!(
            p.on_key(KeyCode::Enter),
            PickerAction::Apply(Rule::KeepLast(12345))
        );
        // « tout sauf la courante » n'a pas de paramètre.
        p.on_key(KeyCode::Down);
        p.on_key(KeyCode::Down);
        p.on_key(KeyCode::Down);
        p.on_key(KeyCode::Backspace);
        p.on_key(KeyCode::Char('4'));
        p.on_key(KeyCode::Right);
        assert_eq!(p.on_key(KeyCode::Char('x')), PickerAction::None);
        assert_eq!(
            p.on_key(KeyCode::Enter),
            PickerAction::Apply(Rule::AllButCurrent)
        );
    }
}
