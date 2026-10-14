use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;

pub const TOKEN_ENV: &str = "KDRIVE_TOKEN";

fn dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "kdvc").context("impossible de déterminer le dossier utilisateur")
}

pub fn token_path() -> Result<PathBuf> {
    Ok(dirs()?.config_dir().join("token"))
}

pub fn cache_db_path() -> Result<PathBuf> {
    let dir = dirs()?.cache_dir().to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("création de {}", dir.display()))?;
    Ok(dir.join("cache.db"))
}

/// Lit le token depuis `KDRIVE_TOKEN`, sinon depuis `~/.config/kdvc/token`.
pub fn load_token() -> Result<String> {
    if let Ok(t) = std::env::var(TOKEN_ENV)
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let path = token_path()?;
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => {
            warn_if_world_readable(&path);
            Ok(t.trim().to_string())
        }
        _ => bail!(
            "aucun token API trouvé.\n\
             Créez un token (scope « drive ») sur https://manager.infomaniak.com/v3/ng/profile/user/token/list\n\
             puis exportez-le dans {TOKEN_ENV} ou enregistrez-le dans {} (chmod 600).",
            path.display()
        ),
    }
}

#[cfg(unix)]
fn warn_if_world_readable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        eprintln!(
            "attention : {} est lisible par d'autres utilisateurs (chmod 600 conseillé)",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_: &std::path::Path) {}
