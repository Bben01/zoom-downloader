use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    Ok,
    Skipped,
    Error,
    Exception,
    NoUrl,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadResult {
    pub status: DownloadStatus, // ok, skipped, error, exception, no_url, cancelled
    pub title: String,
    pub path: Option<String>,
    pub error: Option<String>,
    pub timestamp: String,
}

impl DownloadResult {
    pub fn ok(title: String, path: String) -> Self {
        Self {
            status: DownloadStatus::Ok,
            title,
            path: Some(path),
            error: None,
            timestamp: Utc::now().to_rfc3339(),
        }
    }

    pub fn skipped(title: String) -> Self {
        Self {
            status: DownloadStatus::Skipped,
            title,
            path: None,
            error: None,
            timestamp: Utc::now().to_rfc3339(),
        }
    }

    pub fn error(title: String, error: String) -> Self {
        Self {
            status: DownloadStatus::Error,
            title,
            path: None,
            error: Some(error),
            timestamp: Utc::now().to_rfc3339(),
        }
    }

    pub fn no_url(title: String) -> Self {
        Self {
            status: DownloadStatus::NoUrl,
            title,
            path: None,
            error: Some("No download URL".to_string()),
            timestamp: Utc::now().to_rfc3339(),
        }
    }

    pub fn cancelled(title: String) -> Self {
        Self {
            status: DownloadStatus::Cancelled,
            title,
            path: None,
            error: Some("Download cancelled".to_string()),
            timestamp: Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadReport {
    pub results: Vec<DownloadResult>,
}

impl DownloadReport {
    pub async fn load(report_path: &Path) -> Result<Self> {
        if report_path.exists() {
            let content = fs::read_to_string(report_path)
                .await
                .context("Failed to read report file")?;
            let report: Self =
                serde_json::from_str(&content).context("Failed to parse report JSON")?;
            Ok(report)
        } else {
            Ok(Self { results: Vec::new() })
        }
    }

    pub async fn save(&self, report_path: &Path) -> Result<()> {
        // Create parent directory if needed
        if let Some(parent) = report_path.parent() {
            fs::create_dir_all(parent)
                .await
                .context("Failed to create report directory")?;
        }

        let json = serde_json::to_string_pretty(self).context("Failed to serialize report")?;
        fs::write(report_path, json)
            .await
            .context("Failed to write report file")?;
        Ok(())
    }

    pub fn add_result(&mut self, result: DownloadResult) {
        self.results.push(result);
    }

    pub fn get_successful_titles(&self) -> HashSet<String> {
        self.results
            .iter()
            .filter(|r| r.status == DownloadStatus::Ok || r.status == DownloadStatus::Skipped)
            .map(|r| r.title.clone())
            .collect()
    }

    pub fn get_stats(&self, recent_results: &[DownloadResult]) -> DownloadStats {
        DownloadStats {
            success: recent_results.iter().filter(|r| r.status == DownloadStatus::Ok).count(),
            skipped: recent_results.iter().filter(|r| r.status == DownloadStatus::Skipped).count(),
            errors: recent_results
                .iter()
                .filter(|r| matches!(r.status, DownloadStatus::Error | DownloadStatus::Exception | DownloadStatus::NoUrl))
                .count(),
            total_in_report: self.results.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadStats {
    pub success: usize,
    pub skipped: usize,
    pub errors: usize,
    pub total_in_report: usize,
}
