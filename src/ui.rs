use std::time::Instant;

use chrono::{Local, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Tabs, Wrap,
};

use crate::api::Drive;
use crate::app::{App, Overlay, RULE_CHOICES, RulePicker, SearchForm, Tab, VersionsView};
use crate::cleaner::{ExecSummary, Plan};
use crate::db::Entry;

const ACCENT: Color = Color::Cyan;
const DANGER: Color = Color::Red;

fn highlight() -> Style {
    Style::new()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

// ---------- formatage ----------

pub fn fmt_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["o", "Ko", "Mo", "Go", "To"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v.abs() >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} o")
    } else {
        format!("{v:.1} {}", UNITS[u]).replace('.', ",")
    }
}

pub fn fmt_date(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(d) if ts > 0 => d.format("%Y-%m-%d %H:%M").to_string(),
        _ => "—".into(),
    }
}

pub fn fmt_count(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn fmt_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {:02} min", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{} min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

/// Image courante de l'indicateur de chargement (une toutes les 100 ms).
fn spinner() -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let ms = chrono::Utc::now().timestamp_millis().unsigned_abs();
    FRAMES[(ms / 100 % FRAMES.len() as u64) as usize]
}

fn fmt_pct(ratio: f64) -> String {
    format!("{:.1} %", ratio * 100.0).replace('.', ",")
}

fn block(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).bold(),
        ))
}

/// Pourcentage d'espace occupé par les anciennes versions.
fn ratio_of(part: i64, whole: i64) -> f64 {
    if whole <= 0 {
        0.0
    } else {
        (part as f64 / whole as f64).clamp(0.0, 1.0)
    }
}

// ---------- rendu principal ----------

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, status] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    match app.tab {
        Tab::Stats => draw_stats(f, app, body),
        Tab::Explorer => draw_explorer(f, app, body),
        Tab::Top => {
            let title = format!("Top fichiers par espace de versions ({})", app.top.len());
            draw_entries(f, body, &title, &app.top, &mut app.top_state, true)
        }
        Tab::Search => draw_search(f, app, body),
        Tab::Log => draw_log(f, app, body),
    }
    draw_status(f, app, status);
    draw_overlay(f, app);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let [tabs_area, drive_area] =
        Layout::horizontal([Constraint::Min(40), Constraint::Length(40)]).areas(area);
    let titles = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} {}", i + 1, t.title()));
    let selected = app.tab.index();
    f.render_widget(
        Tabs::new(titles)
            .select(selected)
            .highlight_style(Style::new().fg(ACCENT).bold().underlined())
            .block(Block::new().title(" kDrive Version Cleaner ".bold())),
        tabs_area,
    );
    let drive = match &app.drive {
        Some(d) => {
            let scanned = d
                .scanned_at
                .map(fmt_date)
                .unwrap_or_else(|| "jamais".into());
            Line::from(vec![
                Span::styled(d.name.clone(), Style::new().fg(ACCENT).bold()),
                Span::raw(format!("  scan : {scanned}")).dim(),
                Span::styled(
                    if d.scan_warning.is_some() {
                        " (partiel)"
                    } else {
                        ""
                    },
                    Style::new().fg(Color::Yellow).bold(),
                ),
            ])
        }
        None => Line::from("aucun drive").dim(),
    };
    f.render_widget(Paragraph::new(drive).right_aligned(), drive_area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let [line1, line2] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    if let Some(scan) = &app.scan {
        let ratio = if scan.dirs_total == 0 {
            0.0
        } else {
            scan.dirs_done as f64 / scan.dirs_total as f64
        };
        let elapsed = scan.started.elapsed().as_secs();
        let rate = (scan.files as u64)
            .checked_div(elapsed)
            .map(|r| format!(" ({}/s)", fmt_count(r as i64)))
            .unwrap_or_default();
        let dirs = if scan.dirs_total == 0 {
            "lecture de la racine".to_string()
        } else {
            format!("dossiers {}/{}", scan.dirs_done, scan.dirs_total)
        };
        let label = format!(
            "{} Scan · {} éléments{rate} · {dirs} · {} · {}",
            spinner(),
            fmt_count(scan.files as i64),
            fmt_duration(elapsed),
            scan.current
        );
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .label(label)
                .gauge_style(Style::new().fg(ACCENT).bg(Color::Black)),
            line1,
        );
    } else {
        f.render_widget(Paragraph::new(app.status.as_str()).fg(Color::Yellow), line1);
    }

    let hints = match app.tab {
        Tab::Explorer => {
            "↑↓ naviguer · Entrée ouvrir · ← remonter · c nettoyer l'élément · C nettoyer le dossier"
        }
        Tab::Top => {
            "↑↓ naviguer · Entrée versions · c nettoyer le fichier · C nettoyer tout le top"
        }
        Tab::Search if app.search.editing => "Tab champ suivant · Entrée chercher · Échap terminer",
        Tab::Search => {
            "/ modifier la recherche · Entrée versions · c nettoyer · C nettoyer les résultats"
        }
        Tab::Log => "↑↓ naviguer · e exporter en CSV",
        Tab::Stats => "1-5 onglets",
    };
    let drive_hint = if app.drives.len() > 1 {
        " · d drive"
    } else {
        ""
    };
    f.render_widget(
        Paragraph::new(format!(
            "{hints} · r rescanner{drive_hint} · ? aide · q quitter"
        ))
        .dim(),
        line2,
    );
}

// ---------- onglets ----------

fn draw_stats(f: &mut Frame, app: &App, area: Rect) {
    let outer = block("Statistiques");
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let s = &app.stats;
    let (drive_size, used) = app
        .drive
        .as_ref()
        .map(|d| (d.size, d.used_size))
        .unwrap_or_default();
    let used = if used > 0 {
        used
    } else {
        s.files_size + s.versions_size
    };

    let warning = app.drive.as_ref().and_then(|d| d.scan_warning.clone());
    let [text_area, _, g1, _, g2, _, g3, _, warn_area] = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner.inner(ratatui::layout::Margin::new(2, 1)));

    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::raw(format!("{k:<40}")),
            Span::styled(v, Style::new().bold()),
        ])
    };
    let lines = vec![
        row("Fichiers", fmt_count(s.files)),
        row("Dossiers", fmt_count(s.dirs)),
        row(
            "Taille des fichiers (version courante)",
            fmt_size(s.files_size),
        ),
        row(
            "Fichiers ayant des anciennes versions",
            fmt_count(s.versioned_files),
        ),
        row("Anciennes versions", fmt_count(s.old_versions)),
        Line::from(vec![
            Span::raw(format!("{:<40}", "Espace occupé par les versions")),
            Span::styled(
                fmt_size(s.versions_size),
                Style::new().fg(Color::Yellow).bold(),
            ),
        ]),
        row(
            "Espace utilisé / capacité du drive",
            format!(
                "{} / {}",
                fmt_size(used),
                if drive_size > 0 {
                    fmt_size(drive_size)
                } else {
                    "?".into()
                }
            ),
        ),
        Line::default(),
        Line::from(
            "Le gain réel dépend des règles choisies et des versions « conservées pour toujours ».",
        )
        .dim(),
    ];
    f.render_widget(Paragraph::new(lines), text_area);
    if let Some(w) = warning {
        let text = format!(
            "⚠ Dernier scan partiel : ces dossiers n'ont pas pu être relus : {w}. \
             Appuyez sur r pour réessayer."
        );
        f.render_widget(
            Paragraph::new(text)
                .fg(Color::Yellow)
                .wrap(Wrap { trim: true }),
            warn_area,
        );
    }

    let gauge = |label: String, ratio: f64, color: Color| {
        Gauge::default()
            .ratio(ratio)
            .label(label)
            .gauge_style(Style::new().fg(color).bg(Color::Black))
    };
    if drive_size > 0 {
        f.render_widget(
            gauge(
                format!("Drive utilisé : {}", fmt_pct(ratio_of(used, drive_size))),
                ratio_of(used, drive_size),
                ACCENT,
            ),
            g1,
        );
    }
    f.render_widget(
        gauge(
            format!(
                "Versions / espace utilisé : {}",
                fmt_pct(ratio_of(s.versions_size, used))
            ),
            ratio_of(s.versions_size, used),
            Color::Yellow,
        ),
        g2,
    );
    f.render_widget(
        gauge(
            format!(
                "Fichiers versionnés : {}",
                fmt_pct(ratio_of(s.versioned_files, s.files))
            ),
            ratio_of(s.versioned_files, s.files),
            Color::Magenta,
        ),
        g3,
    );
}

fn draw_explorer(f: &mut Frame, app: &mut App, area: Rect) {
    let title = format!(
        "{}  —  versions : {} dans {} fichier(s)",
        app.cwd.path,
        fmt_size(app.explorer.iter().map(|e| e.versions_size).sum::<i64>()),
        fmt_count(app.explorer.iter().map(|e| e.versioned_files).sum())
    );
    draw_entries(
        f,
        area,
        &title,
        &app.explorer,
        &mut app.explorer_state,
        false,
    );
}

fn draw_entries(
    f: &mut Frame,
    area: Rect,
    title: &str,
    entries: &[Entry],
    state: &mut TableState,
    show_path: bool,
) {
    let max = entries.iter().map(|e| e.versions_size).max().unwrap_or(0);
    let rows = entries.iter().map(|e| {
        let name = if show_path {
            e.path.clone()
        } else if e.is_dir {
            format!("{}/", e.name)
        } else {
            e.name.clone()
        };
        let versions = if e.is_dir {
            format!("{} fich.", fmt_count(e.versioned_files))
        } else if e.nb_versions > 1 {
            fmt_count(e.nb_versions)
        } else {
            String::new()
        };
        let bar_len = if max > 0 {
            ((e.versions_size as f64 / max as f64) * 10.0).round() as usize
        } else {
            0
        };
        let style = if e.is_dir {
            Style::new().fg(Color::Blue).bold()
        } else if e.nb_versions <= 1 {
            Style::new().dim()
        } else {
            Style::new()
        };
        Row::new(vec![
            Cell::from(name),
            Cell::from(versions),
            Cell::from(if e.versions_size > 0 {
                fmt_size(e.versions_size)
            } else {
                String::new()
            })
            .fg(Color::Yellow),
            Cell::from("█".repeat(bar_len)).fg(Color::Yellow),
            Cell::from(fmt_size(e.size)),
            Cell::from(fmt_date(e.last_modified_at)),
        ])
        .style(style)
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(16),
        ],
    )
    .header(
        Row::new(["Nom", "Versions", "Poids vers.", "", "Taille", "Modifié"])
            .style(Style::new().bold().underlined()),
    )
    .row_highlight_style(highlight())
    .highlight_symbol("▶ ")
    .block(block(title));
    f.render_stateful_widget(table, area, state);
    if entries.is_empty() {
        let msg = Paragraph::new("(vide — lancez un scan avec r si nécessaire)")
            .dim()
            .centered();
        f.render_widget(msg, area.centered_vertically(Constraint::Length(1)));
    }
}

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    let [form, results] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    let mut spans = Vec::new();
    for (i, label) in SearchForm::LABELS.iter().enumerate() {
        let focused = app.search.editing && app.search.focus == i;
        let value = &app.search.fields[i];
        spans.push(Span::raw(format!("{label} : ")).dim());
        let shown = if focused {
            format!("{value}▏")
        } else if value.is_empty() {
            "—".into()
        } else {
            value.clone()
        };
        spans.push(Span::styled(
            format!("{shown:<12}"),
            if focused {
                Style::new().fg(Color::Black).bg(ACCENT)
            } else {
                Style::new().bold()
            },
        ));
        spans.push(Span::raw("   "));
    }
    let title = if app.search.editing {
        "Recherche (édition)"
    } else {
        "Recherche  —  / pour modifier"
    };
    f.render_widget(Paragraph::new(Line::from(spans)).block(block(title)), form);
    let title = format!("Résultats ({})", app.search_results.len());
    draw_entries(
        f,
        results,
        &title,
        &app.search_results,
        &mut app.search_state,
        true,
    );
}

fn draw_log(f: &mut Frame, app: &mut App, area: Rect) {
    let ok = app.log.iter().filter(|e| e.ok).count();
    let freed: i64 = app.log.iter().filter(|e| e.ok).map(|e| e.size).sum();
    let title = format!(
        "Journal — {} suppression(s), {} libérés",
        fmt_count(ok as i64),
        fmt_size(freed)
    );
    let rows = app.log.iter().map(|e| {
        Row::new(vec![
            Cell::from(fmt_date(e.ts)),
            Cell::from(if e.ok { "ok" } else { "échec" }).fg(if e.ok {
                Color::Green
            } else {
                DANGER
            }),
            Cell::from(e.path.clone()),
            Cell::from(fmt_date(e.version_created_at)),
            Cell::from(fmt_size(e.size)),
            Cell::from(e.error.clone().unwrap_or_default()).fg(DANGER),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(6),
            Constraint::Min(20),
            Constraint::Length(16),
            Constraint::Length(10),
            Constraint::Length(30),
        ],
    )
    .header(
        Row::new([
            "Date",
            "Statut",
            "Fichier",
            "Version du",
            "Taille",
            "Erreur",
        ])
        .style(Style::new().bold().underlined()),
    )
    .row_highlight_style(highlight())
    .block(block(&title));
    f.render_stateful_widget(table, area, &mut app.log_state);
}

// ---------- fenêtres ----------

fn popup(f: &mut Frame, width: u16, height: u16, title: &str, border: Color) -> Rect {
    let area = f.area().centered(
        Constraint::Length(width.min(f.area().width)),
        Constraint::Length(height.min(f.area().height)),
    );
    f.render_widget(Clear, area);
    let b = block(title).border_style(Style::new().fg(border));
    let inner = b.inner(area);
    f.render_widget(b, area);
    inner
}

fn draw_overlay(f: &mut Frame, app: &mut App) {
    let full_w = f.area().width.saturating_sub(6);
    let full_h = f.area().height.saturating_sub(4);
    match &mut app.overlay {
        Overlay::None => {}
        Overlay::Help => draw_help(f),
        Overlay::DrivePicker(state) => draw_drive_picker(f, &app.drives, state),
        Overlay::Versions(view) => draw_versions(f, view, full_w, full_h),
        Overlay::RulePicker(p) => draw_rule_picker(f, p),
        Overlay::Preparing {
            label,
            done,
            total,
            started,
        } => draw_preparing(f, label, *done, *total, *started),
        Overlay::Preview { plan, state } => draw_preview(f, plan, state, full_w, full_h),
        Overlay::Confirm { plan, input } => draw_confirm(f, plan, input),
        Overlay::Executing {
            done,
            total,
            freed,
            failed,
        } => draw_executing(f, *done, *total, *freed, *failed),
        Overlay::Summary(s) => draw_summary(f, s),
    }
}

fn draw_help(f: &mut Frame) {
    let inner = popup(f, 70, 24, "Aide", ACCENT);
    let lines: Vec<Line> = [
        ("1-5, Tab", "changer d'onglet"),
        ("↑↓ j k, PgUp PgDn, g G", "naviguer"),
        (
            "Entrée / →",
            "ouvrir un dossier, voir les versions d'un fichier",
        ),
        ("← / Retour arrière", "dossier parent"),
        ("c", "nettoyer l'élément sélectionné (fichier ou dossier)"),
        ("C", "nettoyer tout le dossier / top / résultats"),
        (
            "Espace (versions)",
            "cocher une version pour une suppression manuelle",
        ),
        (
            "c (versions)",
            "supprimer les versions cochées, sinon choisir une règle",
        ),
        ("/", "modifier la recherche"),
        ("e (journal)", "exporter le journal en CSV"),
        ("r", "rescanner le drive"),
        ("d", "changer de drive"),
        ("Échap", "fermer / annuler une opération"),
        ("q, Ctrl+C", "quitter"),
        ("", ""),
        ("Sécurité", "la version courante et les versions"),
        (
            "",
            "« conservées pour toujours » ne sont jamais supprimées.",
        ),
        ("", "Toute suppression passe par un aperçu puis « oui »."),
    ]
    .iter()
    .map(|(k, v)| {
        Line::from(vec![
            Span::styled(format!("{k:<24}"), Style::new().fg(ACCENT)),
            Span::raw(*v),
        ])
    })
    .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_drive_picker(f: &mut Frame, drives: &[Drive], state: &mut TableState) {
    let inner = popup(
        f,
        60,
        (drives.len() as u16 + 4).min(20),
        "Choisir un drive",
        ACCENT,
    );
    let rows = drives.iter().map(|d| {
        Row::new(vec![
            Cell::from(d.name.clone()),
            Cell::from(d.used_size.map(fmt_size).unwrap_or_default()),
            Cell::from(format!("#{}", d.id)).dim(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .row_highlight_style(highlight())
    .highlight_symbol("▶ ");
    f.render_stateful_widget(table, inner, state);
}

fn draw_versions(f: &mut Frame, view: &mut VersionsView, full_w: u16, full_h: u16) {
    let title = format!("Versions — {}", view.entry.path);
    let inner = popup(f, full_w.min(100), full_h.min(30), &title, ACCENT);
    let [info, table_area, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(inner);
    f.render_widget(
        Paragraph::new(format!(
            "Taille courante : {} · {} versions · anciennes versions : {}",
            fmt_size(view.entry.size),
            view.entry.nb_versions,
            fmt_size(view.entry.versions_size)
        )),
        info,
    );
    match (&view.versions, &view.error) {
        (_, Some(e)) => f.render_widget(
            Paragraph::new(format!("Erreur : {e}"))
                .fg(DANGER)
                .wrap(Wrap { trim: true }),
            table_area,
        ),
        (None, None) => f.render_widget(
            Paragraph::new(format!("{} Chargement des versions…", spinner())).dim(),
            table_area,
        ),
        (Some(versions), None) => {
            let rows = versions.iter().map(|v| {
                let is_current = Some(v.id) == view.current_id;
                let mark = if view.marked.contains(&v.id) {
                    "[x]"
                } else if is_current || v.keep_forever {
                    "   "
                } else {
                    "[ ]"
                };
                let note = if is_current {
                    "protégée (plus récente)"
                } else if v.keep_forever {
                    "conservée pour toujours"
                } else {
                    ""
                };
                let style = if view.marked.contains(&v.id) {
                    Style::new().fg(DANGER)
                } else if is_current {
                    Style::new().fg(Color::Green)
                } else {
                    Style::new()
                };
                Row::new(vec![
                    Cell::from(mark),
                    Cell::from(fmt_date(v.created_at)),
                    Cell::from(fmt_size(v.size)),
                    Cell::from(note),
                    Cell::from(format!("#{}", v.id)).dim(),
                ])
                .style(style)
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(3),
                    Constraint::Length(16),
                    Constraint::Length(10),
                    Constraint::Length(24),
                    Constraint::Min(8),
                ],
            )
            .header(
                Row::new(["", "Date", "Taille", "", "Id"]).style(Style::new().bold().underlined()),
            )
            .row_highlight_style(highlight())
            .highlight_symbol("▶ ");
            f.render_stateful_widget(table, table_area, &mut view.state);
        }
    }
    let marked: i64 = view
        .versions
        .iter()
        .flatten()
        .filter(|v| view.marked.contains(&v.id))
        .map(|v| v.size)
        .sum();
    let text = if view.marked.is_empty() {
        "Espace cocher · c appliquer une règle · Échap fermer".to_string()
    } else {
        format!(
            "{} cochée(s), {} · c supprimer la sélection · Échap fermer",
            view.marked.len(),
            fmt_size(marked)
        )
    };
    f.render_widget(Paragraph::new(text).dim(), help);
}

fn draw_rule_picker(f: &mut Frame, p: &RulePicker) {
    let inner = popup(f, 72, 13, "Règle de nettoyage", ACCENT);
    let total: i64 = p.targets.iter().map(|e| e.versions_size).sum();
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Cible : ").dim(),
            Span::raw(p.label.clone()).bold(),
        ]),
        Line::from(format!(
            "{} fichier(s) versionné(s), {} d'anciennes versions",
            fmt_count(p.targets.len() as i64),
            fmt_size(total)
        ))
        .dim(),
        Line::default(),
    ];
    for i in 0..RULE_CHOICES {
        let text = match i {
            0 => format!(
                "Garder les ◀ {} ▶ dernières versions (courante comprise)",
                p.keep_n
            ),
            1 => format!("Supprimer les versions de plus de ◀ {} ▶ jours", p.days),
            _ => "Supprimer tout sauf la version courante".into(),
        };
        let selected = i == p.selected;
        lines.push(Line::from(vec![
            Span::raw(if selected { "▶ " } else { "  " }),
            Span::styled(text, if selected { highlight() } else { Style::new() }),
        ]));
    }
    lines.push(Line::default());
    lines.push(
            Line::from("↑↓ choisir · ←→ ou chiffres : paramètre · Entrée : aperçu (rien n'est supprimé) · Échap")
                .dim(),
        );
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_preparing(f: &mut Frame, label: &str, done: usize, total: usize, started: Instant) {
    let inner = popup(f, 70, 7, "Préparation de l'aperçu", ACCENT);
    let [l1, l2, _, l3] = Layout::vertical([Constraint::Length(1); 4]).areas(inner);
    f.render_widget(Paragraph::new(format!("Cible : {label}")), l1);
    let ratio = if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    };
    let elapsed = started.elapsed().as_secs_f64();
    let eta = if done > 0 {
        let secs = elapsed / done as f64 * (total - done) as f64;
        format!(" · reste ~{}", fmt_duration(secs as u64))
    } else {
        format!(
            " · jusqu'à ~{} (60 requêtes/min)",
            fmt_duration(total as u64)
        )
    };
    f.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(format!(
                "{} Chargement des versions {done}/{total}{eta}",
                spinner()
            ))
            .gauge_style(Style::new().fg(ACCENT).bg(Color::Black)),
        l2,
    );
    f.render_widget(Paragraph::new("Échap pour annuler").dim(), l3);
}

fn draw_preview(f: &mut Frame, plan: &Plan, state: &mut TableState, full_w: u16, full_h: u16) {
    let inner = popup(
        f,
        full_w.min(120),
        full_h,
        "Aperçu — rien n'a encore été supprimé",
        Color::Yellow,
    );
    let [info, table_area, help] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(inner);
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Règle : ").dim(),
            Span::raw(plan.rule.clone()).bold(),
        ]),
        Line::from(vec![
            Span::raw(format!(
                "{} version(s) à supprimer dans {} fichier(s) ({} analysés) — espace libéré : ",
                fmt_count(plan.items.len() as i64),
                fmt_count(plan.files_affected as i64),
                fmt_count(plan.files_checked as i64)
            )),
            Span::styled(
                fmt_size(plan.total_size),
                Style::new().fg(Color::Yellow).bold(),
            ),
        ]),
    ];
    if !plan.errors.is_empty() {
        lines.push(
            Line::from(format!(
                "{} fichier(s) ignoré(s) (versions illisibles) : {}",
                plan.errors.len(),
                plan.errors[0]
            ))
            .fg(DANGER),
        );
    }
    f.render_widget(Paragraph::new(lines), info);
    let rows = plan.items.iter().map(|i| {
        Row::new(vec![
            Cell::from(i.path.clone()),
            Cell::from(fmt_date(i.created_at)),
            Cell::from(fmt_size(i.size)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(30),
            Constraint::Length(16),
            Constraint::Length(10),
        ],
    )
    .header(Row::new(["Fichier", "Version du", "Taille"]).style(Style::new().bold().underlined()))
    .row_highlight_style(highlight());
    f.render_stateful_widget(table, table_area, state);
    let text = if plan.items.is_empty() {
        "Rien à supprimer avec cette règle. Échap pour fermer."
    } else {
        "Entrée : passer à la confirmation · Échap : abandonner"
    };
    f.render_widget(Paragraph::new(text).dim(), help);
}

fn draw_confirm(f: &mut Frame, plan: &Plan, input: &str) {
    let inner = popup(f, 64, 9, "Confirmer la suppression", DANGER);
    let lines = vec![
        Line::from(format!(
            "Supprimer définitivement {} version(s) ({}) ?",
            fmt_count(plan.items.len() as i64),
            fmt_size(plan.total_size)
        ))
        .bold(),
        Line::from("Cette opération est irréversible.").fg(DANGER),
        Line::default(),
        Line::from(vec![
            Span::raw("Tapez « oui » puis Entrée : "),
            Span::styled(
                format!("{input}▏"),
                Style::new().fg(Color::Black).bg(Color::Yellow),
            ),
        ]),
        Line::default(),
        Line::from("Échap : revenir à l'aperçu").dim(),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_executing(f: &mut Frame, done: usize, total: usize, freed: i64, failed: usize) {
    let inner = popup(f, 70, 6, "Suppression en cours", DANGER);
    let [l1, _, l2] = Layout::vertical([Constraint::Length(1); 3]).areas(inner);
    let ratio = if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    };
    f.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(format!(
                "{} {done}/{total} · {} libérés · {failed} échec(s) · reste ~{}",
                spinner(),
                fmt_size(freed),
                fmt_duration((total - done) as u64)
            ))
            .gauge_style(Style::new().fg(DANGER).bg(Color::Black)),
        l1,
    );
    f.render_widget(
        Paragraph::new("Échap pour arrêter après la version en cours").dim(),
        l2,
    );
}

fn draw_summary(f: &mut Frame, s: &ExecSummary) {
    let inner = popup(
        f,
        80,
        12,
        "Terminé",
        if s.failed > 0 { DANGER } else { Color::Green },
    );
    let mut lines = vec![
        Line::from(format!(
            "{} version(s) supprimée(s), {} libérés.",
            fmt_count(s.deleted as i64),
            fmt_size(s.freed)
        ))
        .bold(),
    ];
    if s.cancelled {
        lines.push(Line::from("Opération interrompue.").fg(Color::Yellow));
    }
    if s.failed > 0 {
        lines.push(Line::from(format!("{} échec(s) :", s.failed)).fg(DANGER));
    }
    for e in s.errors.iter().take(4) {
        lines.push(Line::from(format!("  {e}")).fg(DANGER));
    }
    lines.push(Line::default());
    lines.push(Line::from("Détails dans l'onglet Journal. Entrée pour fermer.").dim());
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(fmt_size(512), "512 o");
        assert_eq!(fmt_size(1536), "1,5 Ko");
        assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3,0 Go");
    }

    #[test]
    fn counts() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1234567), "1\u{202f}234\u{202f}567");
        assert_eq!(fmt_count(-1000), "-1\u{202f}000");
    }
}
