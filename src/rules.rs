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

/// Version traitée comme courante : la plus récente de la liste.
///
/// On ne sait pas de façon fiable si `/versions` inclut la version courante ;
/// protéger systématiquement la plus récente est sûr, au prix au pire d'une
/// ancienne version conservée en trop.
pub fn current_version_id(versions: &[Version]) -> Option<i64> {
    versions
        .iter()
        .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)))
        .map(|v| v.id)
}

/// Renvoie les versions à supprimer, de la plus récente à la plus ancienne.
///
/// Ne renvoie jamais la version courante ni une version marquée `keep_forever`.
pub fn select_versions(versions: &[Version], rule: &Rule, now: i64) -> Vec<Version> {
    let mut sorted = versions.to_vec();
    sort_newest_first(&mut sorted);
    let current = current_version_id(&sorted);
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
        let out = select_versions(&five(), &Rule::AllButCurrent, NOW);
        assert_eq!(ids(&out), vec![4, 3, 2, 1]);
    }

    #[test]
    fn keep_last_counts_current() {
        assert_eq!(
            ids(&select_versions(&five(), &Rule::KeepLast(1), NOW)),
            vec![4, 3, 2, 1]
        );
        assert_eq!(
            ids(&select_versions(&five(), &Rule::KeepLast(3), NOW)),
            vec![2, 1]
        );
    }

    #[test]
    fn keep_last_zero_behaves_like_one() {
        assert_eq!(
            select_versions(&five(), &Rule::KeepLast(0), NOW),
            select_versions(&five(), &Rule::KeepLast(1), NOW)
        );
    }

    #[test]
    fn keep_last_larger_than_count_deletes_nothing() {
        assert!(select_versions(&five(), &Rule::KeepLast(10), NOW).is_empty());
        assert!(select_versions(&five(), &Rule::KeepLast(5), NOW).is_empty());
    }

    #[test]
    fn older_than() {
        let out = select_versions(&five(), &Rule::OlderThan { days: 30 }, NOW);
        // 30 jours pile n'est pas « plus vieux que 30 jours ».
        assert_eq!(ids(&out), vec![2, 1]);
        let out = select_versions(&five(), &Rule::OlderThan { days: 0 }, NOW);
        assert_eq!(ids(&out), vec![4, 3, 2, 1]);
    }

    #[test]
    fn older_than_never_takes_current() {
        let vs = vec![v(1, 500), v(2, 400)];
        let out = select_versions(&vs, &Rule::OlderThan { days: 30 }, NOW);
        assert_eq!(ids(&out), vec![1]);
    }

    #[test]
    fn keep_forever_is_never_selected() {
        let mut vs = five();
        vs.iter_mut().find(|v| v.id == 2).unwrap().keep_forever = true;
        let out = select_versions(&vs, &Rule::AllButCurrent, NOW);
        assert_eq!(ids(&out), vec![4, 3, 1]);
        let out = select_versions(&vs, &Rule::Manual([2, 3].into()), NOW);
        assert_eq!(ids(&out), vec![3]);
    }

    #[test]
    fn manual_cannot_select_current() {
        let out = select_versions(&five(), &Rule::Manual([5, 1].into()), NOW);
        assert_eq!(ids(&out), vec![1]);
    }

    #[test]
    fn empty_and_single() {
        assert!(select_versions(&[], &Rule::AllButCurrent, NOW).is_empty());
        assert!(select_versions(&[v(1, 100)], &Rule::AllButCurrent, NOW).is_empty());
    }
}
