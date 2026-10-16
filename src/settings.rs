use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "zoom-downloader";
const KEYRING_USER: &str = "client-secret";
const CREDENTIALS_FILE: &str = "zoom-credentials.json";

#[derive(Default, Deserialize)]
#[serde(default)]
struct FileCredentials {
    account_id: String,
    client_id: String,
    client_secret: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub account_id: String,
    pub client_id: String,
    /// Stored in the OS credential store, never in the settings file.
    #[serde(skip)]
    pub client_secret: String,
    pub user_id: String,
    pub output_dir: String,
    pub concurrency: u32,
    pub delete_after: bool,
    pub min_delete_age_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            user_id: "me".into(),
            output_dir: default_output_dir().to_string_lossy().into_owned(),
            concurrency: 3,
            delete_after: false,
            min_delete_age_days: 7,
        }
    }
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "ZoomDownloader", "Zoom Downloader")
}

fn settings_path() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().join("settings.json"))
}

pub fn log_file() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_local_dir().join("logs").join("zoom-downloader.log"))
}

fn default_output_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|u| u.video_dir().or(u.document_dir()).map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Zoom Recordings")
}

fn keyring_entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
}

/// Looks next to the executable first, then in the working directory.
fn read_credentials_file() -> Option<FileCredentials> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let candidates = exe_dir.into_iter().chain(std::env::current_dir().ok());
    for dir in candidates {
        let path = dir.join(CREDENTIALS_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        match serde_json::from_str(&text) {
            Ok(creds) => {
                log::info!("Loaded credentials from {}", path.display());
                return Some(creds);
            }
            Err(e) => log::warn!("Ignoring invalid {}: {e}", path.display()),
        }
    }
    None
}

pub fn load() -> Settings {
    let mut s: Settings = settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    match keyring_entry().and_then(|e| e.get_password()) {
        Ok(secret) => s.client_secret = secret,
        Err(keyring::Error::NoEntry) => {}
        Err(e) => log::warn!("Could not read the client secret from the credential store: {e}"),
    }

    if s.account_id.is_empty() || s.client_id.is_empty() || s.client_secret.is_empty() {
        if let Some(c) = read_credentials_file() {
            let fill = |field: &mut String, value: String| {
                if field.is_empty() {
                    *field = value.trim().to_string();
                }
            };
            fill(&mut s.account_id, c.account_id);
            fill(&mut s.client_id, c.client_id);
            fill(&mut s.client_secret, c.client_secret);
        }
    }

    // Environment variables fill in anything not configured in the app.
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    if s.account_id.is_empty() {
        s.account_id = env("ZOOM_ACCOUNT_ID").unwrap_or_default();
    }
    if s.client_id.is_empty() {
        s.client_id = env("ZOOM_CLIENT_ID").unwrap_or_default();
    }
    if s.client_secret.is_empty() {
        s.client_secret = env("ZOOM_CLIENT_SECRET").unwrap_or_default();
    }
    if s.output_dir.trim().is_empty() {
        s.output_dir = Settings::default().output_dir;
    }
    s.concurrency = s.concurrency.clamp(1, 8);
    s
}

pub fn save(s: &Settings) -> Result<()> {
    let path = settings_path().context("Cannot determine the settings folder")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("Cannot create the settings folder")?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(s)?).context("Cannot write settings")?;

    let entry = keyring_entry().context("Credential store unavailable")?;
    if s.client_secret.is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(e).context("Cannot remove the client secret"),
        }
    } else {
        entry
            .set_password(&s.client_secret)
            .context("Cannot store the client secret in the credential store")?;
    }
    Ok(())
}
