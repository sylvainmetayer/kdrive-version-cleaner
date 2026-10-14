# AGENTS.md

Guide pour les agents de code (Claude Code, Codex, Copilot…) travaillant sur ce dépôt.

## Projet

`kdvc` est une TUI Rust (ratatui) qui affiche l'espace occupé par les anciennes versions des fichiers d'un kDrive Infomaniak et permet de les supprimer en sécurité. Le binaire est mono-utilisateur et local. Il n'a pas de serveur et ne parle qu'à l'API Infomaniak.

- Rust **edition 2024**, **MSRV 1.88** (`rust-version` dans `Cargo.toml`, vérifiée en CI). Pas de fonctionnalité plus récente sans relever la MSRV partout : `Cargo.toml`, `ci.yml`, le badge du README et ce fichier.
- Langue : l'interface, les messages d'erreur, les commentaires et les commits sont **en français**.

## Commandes

```sh
cargo build                         # debug
cargo test                          # tests unitaires (inline) + tests/api.rs (wiremock)
cargo clippy --all-targets          # doit rester sans warning (CI : RUSTFLAGS=-D warnings)
cargo fmt --all                     # formatage obligatoire (vérifié en CI)
cargo deny check                    # failles, licences, sources (deny.toml)
cargo run -- --drive <id|nom> --rescan
```

Variables d'environnement :
- `KDRIVE_TOKEN` : token API Infomaniak (scope drive). Sinon, il est lu dans `~/.config/kdvc/token`.
- `KDVC_ACCOUNT_ID` (ou `--account`) : comptes dont les drives sont listés. Sinon, ils sont lus via `/1/accounts`, ce qui demande le scope `accounts`.
- `KDVC_API_URL` : URL de base de l'API, pour pointer vers un serveur de test.
- `XDG_CACHE_HOME` : permet d'isoler le cache SQLite (`<cache>/kdvc/cache.db`) pendant les essais.

Ne jamais lancer de suppression contre un vrai drive pendant un développement. Pour un essai de bout en bout, utilisez un faux serveur HTTP via `KDVC_API_URL`, et pilotez la TUI dans `tmux` (`send-keys` / `capture-pane`).

## Architecture (`src/`)

| Fichier | Rôle |
|---|---|
| `main.rs` | CLI (clap), chargement du token, liste des drives, boucle d'événements (`tokio::select!` entre crossterm, canal `AppEvent` et tick) |
| `app.rs` | État de l'application (`App`), onglets, overlays (`Overlay`), gestion du clavier, lancement des tâches de fond |
| `ui.rs` | Rendu ratatui pur (aucune logique métier), formatage (`fmt_size`, `fmt_date`, `fmt_count`) |
| `events.rs` | `AppEvent` : messages des tâches tokio vers la boucle UI |
| `api/` | Client HTTP Infomaniak : rate limiter partagé, retries, pagination. Modèles serde dans `models.rs` |
| `db.rs` | Cache SQLite : arborescence, versions, journal des suppressions, agrégats par dossier |
| `scanner.rs` | Scan complet du drive (tâche tokio, dossiers de premier niveau en parallèle) |
| `rules.rs` | Sélection des versions à supprimer. **Fonctions pures**, testées |
| `cleaner.rs` | Aperçu (`prepare`) et exécution (`execute`) d'un nettoyage, chargement des versions avec TTL |
| `config.rs` | Chemins (via `directories`) et lecture du token |
| `lib.rs` | Déclaration des modules, `now()` |

Les tâches longues (scan, chargement des versions, aperçu, suppressions) tournent dans `tokio::spawn`. Elles ouvrent leur **propre** connexion SQLite (`Db::open(&db_path)`) et renvoient leurs résultats par `AppEvent`. Le thread UI possède sa connexion dans `App.db`.

## Invariants de sécurité (à ne jamais casser)

1. **La version courante n'est jamais supprimée.** La plus récente de la liste `/versions` (`rules::current_version_id`) est toujours protégée, car on ne sait pas de façon fiable si l'API inclut la version courante dans cette liste.
2. **Les versions `keep_forever` ne sont jamais supprimées.**
3. **Toute suppression passe par un aperçu**, puis par une confirmation où l'utilisateur tape `oui` (`Overlay::Preview` puis `Overlay::Confirm`). Aucun chemin de code ne doit appeler `cleaner::execute` sans un `Plan` validé.
4. Les suppressions se font **version par version** (`DELETE /2/drive/{d}/files/{f}/versions/{v}`). L'endpoint de suppression en masse n'est pas utilisé.
5. **Un `DELETE` n'est pas rejoué** après un timeout ou une erreur 5xx, car le serveur a pu l'appliquer. Il n'est réessayé qu'après une erreur de connexion ou un 429.
6. Chaque suppression, réussie ou en échec, est **inscrite au journal** (table `deletions`).
7. Une règle dont le paramètre est vide ou invalide est refusée. Elle ne retombe jamais sur la valeur la plus destructrice.

Toute modification de `rules.rs`, `cleaner.rs` ou des appels DELETE doit être accompagnée de tests.

## API Infomaniak : à savoir

- Référence : https://developer.infomaniak.com/docs/api. La spec OpenAPI est à https://developer.infomaniak.com/openapi.json : interrogez-la avec `jq` plutôt que la page HTML.
- **Limite de 60 requêtes/min.** Le `RateLimiter` partagé espace les requêtes d'1 s. Un 429 repousse le limiter pour toutes les tâches, et l'attente `Retry-After` est plafonnée à 120 s.
- `GET /2/drive` exige `account_id` (sinon 422 `validation_failed`). On liste donc les comptes (`/1/accounts`, scope `accounts`), ou on prend ceux passés par `--account`.
- `depth=unlimited` est **refusé sur la racine** (id 1). Le scan liste donc la racine, puis les dossiers de premier niveau en profondeur, `SCAN_CONCURRENCY` (4) à la fois. Le serveur peut mettre plusieurs secondes à répondre pour chaque page, et le rate limiter partagé garantit qu'on reste sous la limite.
- Il existe deux paginations :
  - par **curseur** (`cursor` / `has_more`, sur les listings v3) ;
  - par **page** (`page` / `pages`).

  Un `has_more` sans curseur, ou un curseur déjà vu, déclenche une erreur. Il ne faut jamais tronquer une liste en silence, ni boucler.
- Champ `version` d'un fichier (avec `with=version`) : `number`, `total_size` et `keep_forever`. L'espace des anciennes versions vaut `total_size - size`.

## Cache SQLite

- `replace_tree` remplace l'arborescence d'un drive après un scan. Le contenu des dossiers dont le scan a échoué est conservé, et l'avertissement est stocké dans `drives.scan_warning`.
- Les agrégats des dossiers (`size`, `versions_size`, `versioned_files`) sont recalculés par `recompute_aggregates`. `sync_version_count` les propage aussi aux dossiers parents.
- Les versions en cache expirent au bout d'1 h (`VERSIONS_TTL_SECS`).
- Les filtres de chemin utilisent une comparaison de préfixe exacte (`substr(path, 1, length(?)) = ?`), et **jamais `LIKE`**, qui ignore la casse.
- Les migrations de schéma se font dans `Db::init` : `CREATE TABLE IF NOT EXISTS`, plus des `ALTER TABLE` gardés par `pragma_table_info`.

## Tests

- **Unitaires** : écrits directement dans les fichiers (`#[cfg(test)]`) de `rules.rs`, `db.rs` (SQLite en mémoire via `Db::open_in_memory`), `app.rs` et `ui.rs`.
- **Intégration** (`tests/`), tous contre une API simulée par `wiremock`. Le module `tests/common/mod.rs` fournit le jeu de données, les mocks et `client()`, sans limite de débit.
  - `api.rs` : client HTTP (pagination, 429, erreurs, DELETE non rejoué).
  - `cleaner.rs` : aperçu et exécution d'un nettoyage, journal, annulation, cache des versions.
  - `scanner.rs` : scan complet, scan partiel, échecs.
  - `app.rs` : parcours clavier complet de la TUI, vérifié sur le rendu d'un `TestBackend` ratatui.
- **Couverture** : `cargo llvm-cov --all-targets --summary-only`. Seuls `main.rs` et `config.rs` (terminal réel, fichiers utilisateur) ne sont pas couverts.

## CI/CD (`.github/workflows/`)

- `ci.yml` : fmt, clippy, tests Linux/macOS/Windows, MSRV, build.
- `quality.yml` : cargo-deny, couverture via cargo-llvm-cov, envoyée à Codecov et à SonarQube Cloud (secrets `CODECOV_TOKEN` et `SONAR_TOKEN`).
- `codeql.yml`, `scorecard.yml` : analyses de sécurité.
- `release.yml` : se déclenche sur un tag `vX.Y.Z`, qui doit être égal à `version` dans `Cargo.toml`. Il produit les binaires de 6 cibles, avec SHA-256 et attestation de provenance.
- Dépendances : **Renovate** (`renovate.json`). Dependabot n'est pas utilisé pour les mises à jour.

## Conventions

- Suivez le style existant : modules courts, `anyhow::Result`, erreurs contextualisées (`with_context`), pas d'`unwrap()` hors tests.
- Toute opération longue doit afficher un indicateur animé (`ui::spinner`) et être couverte par `App::loading()`, qui accélère le redessin.
- Une erreur dans un gestionnaire de touche ou d'événement s'affiche dans la barre d'état (`app.status`). Elle ne doit pas fermer la TUI.
- Commits en français, au présent, avec un titre court et une liste à puces si nécessaire.
