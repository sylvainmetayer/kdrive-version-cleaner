use serde::Deserialize;

/// Enveloppe commune des réponses de l'API Infomaniak.
#[derive(Debug, Deserialize)]
pub struct Envelope<T> {
    pub result: String,
    pub data: Option<T>,
    pub error: Option<ApiError>,
    // Pagination par curseur (API v3 « navigator »)
    pub cursor: Option<String>,
    pub has_more: Option<bool>,
    // Pagination par page
    pub page: Option<u32>,
    pub pages: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Drive {
    pub id: i64,
    pub name: String,
    /// Capacité totale du drive (octets).
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub used_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteFile {
    pub id: i64,
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub last_modified_at: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub version: Option<FileVersionInfo>,
}

impl RemoteFile {
    pub fn is_dir(&self) -> bool {
        self.kind.as_deref() == Some("dir")
    }

    pub fn is_trashed(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("trashed") | Some("trash_inherited")
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileVersionInfo {
    #[serde(default)]
    pub number: i64,
    /// Taille du fichier, toutes versions comprises.
    #[serde(default)]
    pub total_size: Option<i64>,
    #[serde(default)]
    pub keep_forever: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Version {
    pub id: i64,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub keep_forever: bool,
}
