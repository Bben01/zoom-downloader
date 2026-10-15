use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "zoom-downloader-history.json";

/// Remembers what was downloaded into an output folder, keyed by Zoom file id.
#[derive(Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    files: BTreeMap<String, FileRecord>,
    #[serde(default)]
    trashed_meetings: BTreeMap<String, DateTime<Utc>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub meeting_uuid: String,
    pub topic: String,
    pub file_name: String,
    pub size: u64,
    pub downloaded_at: DateTime<Utc>,
}

impl History {
    fn path(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME)
    }

    pub fn load(dir: &Path) -> Self {
        let path = Self::path(dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("Download history was unreadable ({e}); a backup was kept and a new one started");
                let _ = std::fs::rename(&path, path.with_extension("json.bak"));
                Self::default()
            }
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).context("Cannot create output folder")?;
        let path = Self::path(dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, json).context("Cannot write download history")?;
        std::fs::rename(&tmp, &path).context("Cannot update download history")?;
        Ok(())
    }

    pub fn has_file(&self, file_id: &str) -> bool {
        self.files.contains_key(file_id)
    }

    pub fn record_file(&mut self, file_id: &str, record: FileRecord) {
        self.files.insert(file_id.to_string(), record);
    }

    pub fn record_trashed(&mut self, meeting_uuid: &str) {
        self.trashed_meetings.insert(meeting_uuid.to_string(), Utc::now());
    }
}
