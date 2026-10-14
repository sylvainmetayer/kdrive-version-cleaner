# kdvc — kDrive Version Cleaner

Une petite application en terminal (TUI, en Rust avec ratatui) pour voir quelle place prennent les anciennes versions des fichiers d'un kDrive Infomaniak, puis les supprimer sans risque.

## Installation

```sh
cargo install --path .
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
- **Scan** : la racine est d'abord listée seule, car l'API refuse `depth=unlimited` sur la racine. Chaque dossier de premier niveau est ensuite listé en profondeur, par pages de 1000 éléments.
- **Chargement des versions** : il se fait à la demande, quand on ouvre un fichier ou qu'on calcule un aperçu. Cela coûte une requête par fichier, et une ETA est affichée.
- **Repérage de la version courante** : on compare le nombre de versions annoncé par le fichier (`version.number`) à la longueur de la liste `/versions`.
  - Si la liste a un élément de moins, la version courante n'en fait pas partie.
  - Sinon, on considère que la plus récente de la liste est la version courante, et elle est protégée.

## Développement

```sh
cargo test     # règles, cache SQLite, client HTTP (wiremock)
cargo clippy --all-targets
```

`KDVC_API_URL` permet de faire pointer l'outil vers un serveur de test plutôt que vers `https://api.infomaniak.com`.
