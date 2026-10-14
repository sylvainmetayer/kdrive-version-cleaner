# kdvc — kDrive Version Cleaner

[![CI](https://github.com/sylvainmetayer/kdrive-version-cleaner/actions/workflows/ci.yml/badge.svg)](https://github.com/sylvainmetayer/kdrive-version-cleaner/actions/workflows/ci.yml)
[![Qualité](https://github.com/sylvainmetayer/kdrive-version-cleaner/actions/workflows/quality.yml/badge.svg)](https://github.com/sylvainmetayer/kdrive-version-cleaner/actions/workflows/quality.yml)
[![CodeQL](https://github.com/sylvainmetayer/kdrive-version-cleaner/actions/workflows/codeql.yml/badge.svg)](https://github.com/sylvainmetayer/kdrive-version-cleaner/actions/workflows/codeql.yml)
[![Release](https://img.shields.io/github/v/release/sylvainmetayer/kdrive-version-cleaner?logo=github&sort=semver)](https://github.com/sylvainmetayer/kdrive-version-cleaner/releases/latest)
[![Téléchargements](https://img.shields.io/github/downloads/sylvainmetayer/kdrive-version-cleaner/total?logo=github)](https://github.com/sylvainmetayer/kdrive-version-cleaner/releases)

[![codecov](https://codecov.io/gh/sylvainmetayer/kdrive-version-cleaner/graph/badge.svg)](https://codecov.io/gh/sylvainmetayer/kdrive-version-cleaner)
[![Quality Gate](https://sonarcloud.io/api/project_badges/measure?project=sylvainmetayer_kdrive-version-cleaner&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=sylvainmetayer_kdrive-version-cleaner)
[![Maintainability](https://sonarcloud.io/api/project_badges/measure?project=sylvainmetayer_kdrive-version-cleaner&metric=sqale_rating)](https://sonarcloud.io/summary/new_code?id=sylvainmetayer_kdrive-version-cleaner)
[![Security](https://sonarcloud.io/api/project_badges/measure?project=sylvainmetayer_kdrive-version-cleaner&metric=security_rating)](https://sonarcloud.io/summary/new_code?id=sylvainmetayer_kdrive-version-cleaner)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/sylvainmetayer/kdrive-version-cleaner/badge)](https://scorecard.dev/viewer/?uri=github.com/sylvainmetayer/kdrive-version-cleaner)

[![Licence MIT](https://img.shields.io/github/license/sylvainmetayer/kdrive-version-cleaner)](LICENSE)
[![MSRV](https://img.shields.io/badge/rustc-1.88+-orange?logo=rust)](https://blog.rust-lang.org/)
[![Built With Ratatui](https://img.shields.io/badge/Built_With_Ratatui-000?logo=ratatui&logoColor=fff)](https://ratatui.rs/)
[![Dependabot](https://img.shields.io/badge/dependabot-enabled-025e8c?logo=dependabot)](https://github.com/sylvainmetayer/kdrive-version-cleaner/network/updates)

Une petite application en terminal (TUI, en Rust avec ratatui) pour voir quelle place prennent les anciennes versions des fichiers d'un kDrive Infomaniak, puis les supprimer sans risque.

## Installation

Binaires prêts à l'emploi (Linux x86_64/ARM64/musl, macOS Intel/Apple Silicon, Windows) sur la [page des releases](https://github.com/sylvainmetayer/kdrive-version-cleaner/releases/latest), avec leur somme SHA-256 et une attestation de provenance (`gh attestation verify <archive> -R sylvainmetayer/kdrive-version-cleaner`).

Depuis les sources (Rust ≥ 1.88) :

```sh
cargo install --git https://github.com/sylvainmetayer/kdrive-version-cleaner
```

## Token API

1. Créez un token sur https://manager.infomaniak.com/v3/ng/profile/user/token/list, avec le scope **drive**.
2. Mettez-le à disposition de l'outil, au choix :
   - dans la variable d'environnement : `export KDRIVE_TOKEN=...`
   - dans un fichier :
     ```sh
     mkdir -p ~/.config/kdvc && echo "$TOKEN" > ~/.config/kdvc/token && chmod 600 ~/.config/kdvc/token
     ```

## Utilisation

```sh
kdvc                    # choisit le drive (s'il y en a plusieurs), scanne au premier lancement
kdvc --drive "Mon kDrive" --rescan
```

| Onglet | Contenu |
|---|---|
| 1 Stats | Nombre de fichiers, espace occupé par les anciennes versions, taux d'utilisation |
| 2 Explorateur | Arborescence, avec le poids des versions cumulé par dossier |
| 3 Top fichiers | Les fichiers dont les versions prennent le plus de place |
| 4 Recherche | Filtres par nom, extension, taille minimale et nombre minimal de versions |
| 5 Journal | Historique des suppressions (`e` pour l'exporter en CSV) |

Pour nettoyer :
1. Appuyez sur `c` pour l'élément sélectionné (fichier ou dossier), ou sur `C` pour tout le dossier, tout le top ou tous les résultats de recherche.
2. Choisissez une règle :
   - garder les N dernières versions ;
   - supprimer les versions de plus de X jours ;
   - tout supprimer sauf la version courante.

   Pour une sélection manuelle, ouvrez les versions d'un fichier (`Entrée`), cochez-les avec `Espace`, puis appuyez sur `c`.
3. Un **aperçu** affiche les versions concernées et l'espace libéré. À ce stade, rien n'est encore supprimé.
4. Pour confirmer, tapez `oui`. Les versions sont alors supprimées une par une, et chaque suppression est inscrite au journal. `Échap` interrompt l'opération.

L'outil ne supprime jamais la version courante, ni une version marquée « conservée pour toujours ».

## Fonctionnement

- **Cache** : `~/.cache/kdvc/cache.db` (SQLite). Il contient l'arborescence, les versions déjà chargées et le journal. `r` relance un scan.
- **Limite de l'API** : 60 requêtes par minute. Le client attend au moins 1 s entre deux requêtes et réessaie après une erreur 429 ou 5xx.
- **Scan** : la racine est d'abord listée seule, car l'API refuse `depth=unlimited` sur la racine. Chaque dossier de premier niveau est ensuite listé en profondeur, par pages de 1000 éléments. Si un dossier ne peut pas être listé, il garde son contenu du scan précédent et un avertissement reste affiché dans l'onglet Stats jusqu'au prochain scan complet.
- **Chargement des versions** : il se fait à la demande, quand on ouvre un fichier ou qu'on calcule un aperçu. Cela coûte une requête par fichier, et une ETA est affichée.
- **Version courante** : la plus récente de la liste `/versions` est toujours traitée comme courante et protégée. Si l'API n'inclut pas la version courante dans cette liste, on conserve au pire une ancienne version de trop.

## Développement

```sh
cargo test     # règles, cache SQLite, client HTTP (wiremock)
cargo clippy --all-targets
cargo deny check   # failles, licences et sources des dépendances
```

### CI/CD

| Workflow | Déclencheur | Rôle |
|---|---|---|
| `ci.yml` | push, PR | fmt, clippy, tests Linux/macOS/Windows, MSRV 1.88, build release |
| `quality.yml` | push, PR, hebdo | cargo-deny, couverture (cargo-llvm-cov → Codecov), SonarQube Cloud |
| `codeql.yml` | push, PR, hebdo | analyse CodeQL du code Rust et des workflows |
| `scorecard.yml` | push, hebdo | OpenSSF Scorecard |
| `release.yml` | tag `vX.Y.Z` | binaires 6 plateformes, SHA-256, attestation, release GitHub |

Pour publier une release : passer `version` à X.Y.Z dans `Cargo.toml`, commiter, puis `git tag vX.Y.Z && git push --tags`.

Secrets à configurer : `CODECOV_TOKEN` et `SONAR_TOKEN`. Sans eux, les étapes correspondantes sont simplement ignorées.

`KDVC_API_URL` permet de faire pointer l'outil vers un serveur de test plutôt que vers `https://api.infomaniak.com`.

## Licence

[MIT](LICENSE)
