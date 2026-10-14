pub mod models;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, sleep_until};

use models::Envelope;
pub use models::{Account, Drive, RemoteFile, Version};

pub const DEFAULT_BASE_URL: &str = "https://api.infomaniak.com";
/// L'API Infomaniak limite à 60 requêtes par minute.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(1000);
const MAX_ATTEMPTS: u32 = 6;
/// Plafond d'attente entre deux essais, quel que soit le `Retry-After` reçu.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(120);
pub const ROOT_DIR_ID: i64 = 1;

/// Espace les requêtes d'au moins `interval`.
struct RateLimiter {
    state: Mutex<LimiterState>,
    interval: Duration,
}

struct LimiterState {
    /// Prochain créneau libre.
    next: Instant,
    /// Aucune requête avant cet instant (fixé par un 429).
    blocked_until: Instant,
}

impl RateLimiter {
    async fn acquire(&self) {
        loop {
            let at = {
                let mut st = self.state.lock().await;
                let at = st.next.max(st.blocked_until).max(Instant::now());
                st.next = at + self.interval;
                at
            };
            sleep_until(at).await;
            // Un 429 reçu pendant l'attente repousse aussi les créneaux déjà réservés.
            if self.state.lock().await.blocked_until <= Instant::now() {
                return;
            }
        }
    }

    /// Repousse toutes les requêtes (y compris celles des autres tâches) d'au moins `wait`.
    async fn defer(&self, wait: Duration) {
        let mut st = self.state.lock().await;
        st.blocked_until = st.blocked_until.max(Instant::now() + wait);
        st.next = st.next.max(st.blocked_until);
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    limiter: Arc<RateLimiter>,
}

pub struct CursorPage<T> {
    pub items: Vec<T>,
    pub cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Clone, Copy)]
pub enum Depth {
    Child,
    Unlimited,
}

impl Client {
    pub fn new(token: String) -> Result<Self> {
        Self::with_options(token, DEFAULT_BASE_URL.into(), DEFAULT_MIN_INTERVAL)
    }

    pub fn with_options(token: String, base: String, min_interval: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kdvc/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
            limiter: Arc::new(RateLimiter {
                state: Mutex::new(LimiterState {
                    next: Instant::now(),
                    blocked_until: Instant::now(),
                }),
                interval: min_interval,
            }),
        })
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Envelope<T>> {
        let url = format!("{}{}", self.base, path);
        // Un DELETE peut avoir été traité malgré un timeout ou une 5xx : le rejouer
        // échouerait (version déjà supprimée) et fausserait journal et bilan.
        // On ne rejoue alors que les cas où la requête n'a sûrement pas été traitée.
        let idempotent = method == Method::GET;
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.limiter.acquire().await;
            let resp = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.token)
                .query(query)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e)
                    if attempt < MAX_ATTEMPTS
                        && (e.is_connect() || (idempotent && e.is_timeout())) =>
                {
                    sleep(backoff(attempt)).await;
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("{method} {path}")),
            };

            let status = resp.status();
            if (status == StatusCode::TOO_MANY_REQUESTS || (idempotent && status.is_server_error()))
                && attempt < MAX_ATTEMPTS
            {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| backoff(attempt))
                    .min(MAX_RETRY_WAIT);
                if status == StatusCode::TOO_MANY_REQUESTS {
                    // La limite est globale au token : les autres tâches doivent attendre aussi.
                    self.limiter.defer(wait).await;
                } else {
                    sleep(wait).await;
                }
                continue;
            }

            let body = resp
                .text()
                .await
                .with_context(|| format!("{method} {path} : lecture de la réponse"))?;
            let env: Envelope<T> = serde_json::from_str(&body).with_context(|| {
                format!(
                    "{method} {path} : réponse illisible (HTTP {status}) : {}",
                    truncate(&body, 200)
                )
            })?;
            if env.result != "success" || !status.is_success() {
                let err = env
                    .error
                    .map(|e| format!("{} {}", e.code, e.description.unwrap_or_default()))
                    .unwrap_or_else(|| truncate(&body, 200).to_string());
                bail!("{method} {path} : HTTP {status} : {err}");
            }
            return Ok(env);
        }
    }

    /// Comptes (organisations) de l'utilisateur. Demande le scope `accounts`.
    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.paged("/1/accounts", &[]).await
    }

    /// Drives accessibles d'un compte (`account_id` est obligatoire pour l'API).
    pub async fn list_drives(&self, account_id: i64) -> Result<Vec<Drive>> {
        self.paged("/2/drive", &[("account_id", account_id.to_string())])
            .await
    }

    /// Toutes les pages d'une liste paginée par `page` / `pages`.
    async fn paged<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut page = 1;
        loop {
            let mut query = params.to_vec();
            query.push(("page", page.to_string()));
            query.push(("per_page", "100".into()));
            let env: Envelope<Vec<T>> = self.request(Method::GET, path, &query).await?;
            items.extend(env.data.unwrap_or_default());
            if page >= env.pages.unwrap_or(1) {
                break;
            }
            page += 1;
        }
        Ok(items)
    }

    /// Une page d'éléments d'un dossier. `depth=unlimited` n'est pas accepté sur la racine.
    pub async fn list_files(
        &self,
        drive_id: i64,
        dir_id: i64,
        depth: Depth,
        cursor: Option<&str>,
    ) -> Result<CursorPage<RemoteFile>> {
        let mut query = vec![
            ("with", "version".to_string()),
            ("limit", "1000".into()),
            (
                "depth",
                match depth {
                    Depth::Child => "child",
                    Depth::Unlimited => "unlimited",
                }
                .into(),
            ),
        ];
        if let Some(c) = cursor {
            query.push(("cursor", c.to_string()));
        }
        let env: Envelope<Vec<RemoteFile>> = self
            .request(
                Method::GET,
                &format!("/3/drive/{drive_id}/files/{dir_id}/files"),
                &query,
            )
            .await?;
        let has_more = env.has_more.unwrap_or(false);
        if has_more && env.cursor.is_none() {
            bail!(
                "/3/drive/{drive_id}/files/{dir_id}/files : has_more sans curseur, liste incomplète"
            );
        }
        Ok(CursorPage {
            items: env.data.unwrap_or_default(),
            has_more,
            cursor: env.cursor,
        })
    }

    pub async fn list_versions(&self, drive_id: i64, file_id: i64) -> Result<Vec<Version>> {
        // Accepte les deux paginations de l'API : par page (`pages`) et par curseur
        // (`cursor` / `has_more`, propre à la v3). Ignorer l'une tronquerait la liste.
        let mut versions = Vec::new();
        let mut page = 1;
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let mut query = vec![
                ("per_page", "100".to_string()),
                ("limit", "100".into()),
                ("total", "true".into()),
            ];
            match &cursor {
                Some(c) => query.push(("cursor", c.clone())),
                None => query.push(("page", page.to_string())),
            }
            let env: Envelope<Vec<Version>> = self
                .request(
                    Method::GET,
                    &format!("/3/drive/{drive_id}/files/{file_id}/versions"),
                    &query,
                )
                .await?;
            versions.extend(env.data.unwrap_or_default());
            let path = format!("/3/drive/{drive_id}/files/{file_id}/versions");
            if env.has_more == Some(true) {
                match env.cursor {
                    // Un curseur déjà vu (même plus tôt qu'à la page précédente) bouclerait.
                    Some(c) if !seen_cursors.insert(c.clone()) => {
                        bail!("{path} : curseur répété, pagination bloquée")
                    }
                    Some(c) => {
                        cursor = Some(c);
                        continue;
                    }
                    // Réponse paginée par page qui annonce aussi has_more.
                    None if cursor.is_none() && page < env.pages.unwrap_or(1) => {}
                    None => bail!("{path} : has_more sans curseur, liste incomplète"),
                }
            }
            if cursor.is_some() || page >= env.pages.unwrap_or(1) {
                break;
            }
            page += 1;
        }
        Ok(versions)
    }

    pub async fn delete_version(&self, drive_id: i64, file_id: i64, version_id: i64) -> Result<()> {
        let env: Envelope<serde_json::Value> = self
            .request(
                Method::DELETE,
                &format!("/2/drive/{drive_id}/files/{file_id}/versions/{version_id}"),
                &[],
            )
            .await?;
        match env.data {
            Some(serde_json::Value::Bool(false)) => Err(anyhow!("l'API a refusé la suppression")),
            _ => Ok(()),
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(5 * 2u64.pow(attempt.saturating_sub(1).min(4)))
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defer_also_delays_already_reserved_slots() {
        let limiter = Arc::new(RateLimiter {
            state: Mutex::new(LimiterState {
                next: Instant::now(),
                blocked_until: Instant::now(),
            }),
            interval: Duration::from_millis(50),
        });
        let start = Instant::now();
        limiter.acquire().await;
        // Créneau réservé pour t+50 ms, puis un 429 bloque tout jusqu'à t+300 ms.
        let waiting = tokio::spawn({
            let l = limiter.clone();
            async move { l.acquire().await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        limiter.defer(Duration::from_millis(300)).await;
        waiting.await.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "{:?}",
            start.elapsed()
        );
    }
}
