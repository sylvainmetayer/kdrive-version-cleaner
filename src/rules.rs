//! Sélection des versions à supprimer. Fonctions pures, sans accès réseau ni base.

use std::collections::HashSet;

use crate::api::Version;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// Versions cochées à la main.
    Manual(HashSet<i64>),
    /// Garde les N versions les plus récentes, version courante comprise.
    KeepLast(usize),
    /// Versions créées il y a plus de N jours.
    OlderThan { days: u32 },
    /// Toutes les anciennes versions.
    AllButCurrent,
}

impl Rule {
    pub fn label(&self) -> String {
        match self {
            Rule::Manual(ids) => format!("sélection manuelle ({} versions)", ids.len()),
            Rule::KeepLast(n) => format!("garder les {n} dernières versions"),
            Rule::OlderThan { days } => format!("versions de plus de {days} jours"),
            Rule::AllButCurrent => "tout sauf la version courante".into(),
        }
    }
}

/// Trie les versions de la plus récente à la plus ancienne.
pub fn sort_newest_first(versions: &mut [Version]) {
    versions.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
}

/// Anciennes versions d'un fichier selon le champ `version` du listing (au scan) :
/// `number` et `total_size - size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanVersions {
    pub count: i64,
    pub size: i64,
    /// Taille de la version courante.
    pub file_size: i64,
}

/// Vrai si la liste `/versions` correspond exactement aux anciennes versions vues
/// au scan : même nombre et même taille cumulée. La version courante n'y figure
/// alors pas (elle aurait ajouté une entrée et sa taille).
///
/// Un fichier courant vide ne permet pas de conclure : sa taille ne changerait
/// pas la somme.
fn matches_scan(versions: &[Version], scan: Option<ScanVersions>) -> bool {
    let Some(scan) = scan else { return false };
    let size: i64 = versions.iter().map(|v| v.size).sum();
    scan.file_size > 0 && versions.len() as i64 == scan.count && size == scan.size
}

/// Version de la liste traitée comme courante, donc protégée.
///
/// L'API ne renvoie que les anciennes versions dans `/versions`, mais on ne s'y fie
/// que si la liste correspond exactement au scan (`matches_scan`) : on renvoie
/// alors `None`. Sinon (scan absent, périmé ou incohérent), la plus récente de la
/// liste est protégée : au pire une ancienne version est conservée en trop.
pub fn current_version_id(versions: &[Version], scan: Option<ScanVersions>) -> Option<i64> {
    if matches_scan(versions, scan) {
        return None;
    }
    versions
        .iter()
        .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)))
        .map(|v| v.id)
}

/// Renvoie les versions à supprimer, de la plus récente à la plus ancienne.
///
/// Ne renvoie jamais la version courante ni une version marquée `keep_forever`.
pub fn select_versions(
    versions: &[Version],
    scan: Option<ScanVersions>,
    rule: &Rule,
    now: i64,
) -> Vec<Version> {
    let mut sorted = versions.to_vec();
    sort_newest_first(&mut sorted);
    let current = current_version_id(&sorted, scan);
    // Anciennes versions, de la plus récente à la plus ancienne.
    let old: Vec<&Version> = sorted.iter().filter(|v| Some(v.id) != current).collect();

    old.into_iter()
        .enumerate()
        .filter(|(rank, v)| match rule {
            Rule::Manual(ids) => ids.contains(&v.id),
            // La version courante occupe la première des N places.
            Rule::KeepLast(n) => *rank + 1 >= (*n).max(1),
            Rule::OlderThan { days } => v.created_at < now - i64::from(*days) * 86_400,
            Rule::AllButCurrent => true,
        })
        .map(|(_, v)| v)
        .filter(|v| !v.keep_forever)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_000 * DAY;

    fn v(id: i64, age_days: i64) -> Version {
        Version {
            id,
            size: 100,
            created_at: NOW - age_days * DAY,
            keep_forever: false,
        }
    }

    /// 5 versions : id 5 = la plus récente (courante), id 1 = la plus ancienne.
    fn five() -> Vec<Version> {
        vec![v(3, 30), v(1, 400), v(5, 0), v(2, 200), v(4, 10)]
    }

    fn ids(vs: &[Version]) -> Vec<i64> {
        vs.iter().map(|v| v.id).collect()
    }

    #[test]
    fn all_but_current_protects_newest() {
        let out = select_versions(&five(), None, &Rule::AllButCurrent, NOW);
        assert_eq!(ids(&out), vec![4, 3, 2, 1]);
    }

    #[test]
    fn keep_last_counts_current() {
        assert_eq!(
            ids(&select_versions(&five(), None, &Rule::KeepLast(1), NOW)),
            vec![4, 3, 2, 1]
        );
        assert_eq!(
            ids(&select_versions(&five(), None, &Rule::KeepLast(3), NOW)),
            vec![2, 1]
        );
    }

    #[test]
    fn keep_last_zero_behaves_like_one() {
        assert_eq!(
            select_versions(&five(), None, &Rule::KeepLast(0), NOW),
            select_versions(&five(), None, &Rule::KeepLast(1), NOW)
        );
    }

    #[test]
    fn keep_last_larger_than_count_deletes_nothing() {
        assert!(select_versions(&five(), None, &Rule::KeepLast(10), NOW).is_empty());
        assert!(select_versions(&five(), None, &Rule::KeepLast(5), NOW).is_empty());
    }

    #[test]
    fn older_than() {
        let out = select_versions(&five(), None, &Rule::OlderThan { days: 30 }, NOW);
        // 30 jours pile n'est pas « plus vieux que 30 jours ».
        assert_eq!(ids(&out), vec![2, 1]);
        let out = select_versions(&five(), None, &Rule::OlderThan { days: 0 }, NOW);
        assert_eq!(ids(&out), vec![4, 3, 2, 1]);
    }

    #[test]
    fn older_than_never_takes_current() {
        let vs = vec![v(1, 500), v(2, 400)];
        let out = select_versions(&vs, None, &Rule::OlderThan { days: 30 }, NOW);
        assert_eq!(ids(&out), vec![1]);
    }

    #[test]
    fn keep_forever_is_never_selected() {
        let mut vs = five();
        vs.iter_mut().find(|v| v.id == 2).unwrap().keep_forever = true;
        let out = select_versions(&vs, None, &Rule::AllButCurrent, NOW);
        assert_eq!(ids(&out), vec![4, 3, 1]);
        let out = select_versions(&vs, None, &Rule::Manual([2, 3].into()), NOW);
        assert_eq!(ids(&out), vec![3]);
    }

    #[test]
    fn manual_cannot_select_current() {
        let out = select_versions(&five(), None, &Rule::Manual([5, 1].into()), NOW);
        assert_eq!(ids(&out), vec![1]);
    }

    #[test]
    fn empty_and_single() {
        assert!(select_versions(&[], None, &Rule::AllButCurrent, NOW).is_empty());
        assert!(select_versions(&[v(1, 100)], None, &Rule::AllButCurrent, NOW).is_empty());
    }
    /// Scan cohérent avec `five()` : 5 anciennes versions de 100 octets.
    fn scan_five() -> Option<ScanVersions> {
        Some(ScanVersions {
            count: 5,
            size: 500,
            file_size: 100,
        })
    }

    #[test]
    fn list_matching_scan_has_no_current() {
        assert_eq!(current_version_id(&five(), scan_five()), None);
        let out = select_versions(&five(), scan_five(), &Rule::AllButCurrent, NOW);
        assert_eq!(ids(&out), vec![5, 4, 3, 2, 1]);
        // La courante, hors liste, occupe la première des N places.
        let out = select_versions(&five(), scan_five(), &Rule::KeepLast(2), NOW);
        assert_eq!(ids(&out), vec![4, 3, 2, 1]);
        let out = select_versions(&[v(1, 100)], scan_one(), &Rule::AllButCurrent, NOW);
        assert_eq!(ids(&out), vec![1]);
    }

    fn scan_one() -> Option<ScanVersions> {
        Some(ScanVersions {
            count: 1,
            size: 100,
            file_size: 100,
        })
    }

    #[test]
    fn mismatching_scan_protects_newest() {
        let base = scan_five().unwrap();
        for scan in [
            None,
            // Une version de plus dans la liste : la courante peut y figurer.
            Some(ScanVersions {
                count: 4,
                size: 400,
                ..base
            }),
            // Même nombre, taille différente : scan périmé.
            Some(ScanVersions { size: 499, ..base }),
            Some(ScanVersions { count: 6, ..base }),
            // Fichier courant vide : la somme ne prouve rien.
            Some(ScanVersions {
                file_size: 0,
                ..base
            }),
        ] {
            assert_eq!(current_version_id(&five(), scan), Some(5), "{scan:?}");
            let out = select_versions(&five(), scan, &Rule::AllButCurrent, NOW);
            assert_eq!(ids(&out), vec![4, 3, 2, 1], "{scan:?}");
        }
    }

    #[test]
    fn keep_forever_is_protected_even_when_list_matches_scan() {
        let mut vs = five();
        vs.iter_mut().find(|v| v.id == 5).unwrap().keep_forever = true;
        let out = select_versions(&vs, scan_five(), &Rule::Manual([5, 4].into()), NOW);
        assert_eq!(ids(&out), vec![4]);
    }
}
