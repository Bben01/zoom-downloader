use crate::report::DownloadResult;
use crate::zoom_api::ZoomRecord;
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;

pub struct DownloadManager {
    token: String,
    out_dir: PathBuf,
    concurrency: usize,
    client: reqwest::Client,
    shutdown: Arc<AtomicBool>,
    progress_bytes: Arc<AtomicU64>,
}

impl DownloadManager {
    pub fn new(token: String, out_dir: PathBuf, concurrency: usize) -> Self {
        Self {
            token,
            out_dir,
            concurrency,
            client: reqwest::Client::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            progress_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn reset_progress(&self) {
        self.progress_bytes.store(0, Ordering::SeqCst);
    }

    pub fn get_progress_bytes(&self) -> u64 {
        self.progress_bytes.load(Ordering::SeqCst)
    }

    pub async fn download_batch(
        &self,
        recordings: Vec<ZoomRecord>,
        progress_callback: Option<Arc<dyn Fn(u64, usize, usize) + Send + Sync>>,
    ) -> Result<Vec<DownloadResult>> {
        if recordings.is_empty() {
            return Ok(Vec::new());
        }

        // Create output directory
        fs::create_dir_all(&self.out_dir)
            .await
            .context("Failed to create output directory")?;

        let _total_count = recordings.len();
        let completed_count = Arc::new(AtomicU64::new(0));
        let failed_count = Arc::new(AtomicU64::new(0));

        // Download with concurrency limit
        let results: Vec<DownloadResult> = stream::iter(recordings)
            .map(|recording| {
                let client = self.client.clone();
                let token = self.token.clone();
                let out_dir = self.out_dir.clone();
                let shutdown = self.shutdown.clone();
                let progress_bytes = self.progress_bytes.clone();
                let completed = completed_count.clone();
                let failed = failed_count.clone();
                let callback = progress_callback.clone();

                async move {
                    if shutdown.load(Ordering::SeqCst) {
                        return DownloadResult::cancelled(recording.title.clone());
                    }

                    let result =
                        download_one(client, token, out_dir, recording, shutdown, progress_bytes.clone(), callback.clone(), completed.clone(), failed.clone())
                            .await;

                    match result.status.as_str() {
                        "ok" => completed.fetch_add(1, Ordering::SeqCst),
                        "error" | "exception" | "no_url" => failed.fetch_add(1, Ordering::SeqCst),
                        _ => 0,
                    };

                    // Call progress callback if provided
                    if let Some(ref cb) = callback {
                        let bytes = progress_bytes.load(Ordering::SeqCst);
                        let completed_val = completed.load(Ordering::SeqCst) as usize;
                        let failed_val = failed.load(Ordering::SeqCst) as usize;
                        cb(bytes, completed_val, failed_val);
                    }

                    result
                }
            })
            .buffer_unordered(self.concurrency)
            .collect()
            .await;

        Ok(results)
    }
}

async fn download_one(
    client: reqwest::Client,
    token: String,
    out_dir: PathBuf,
    recording: ZoomRecord,
    shutdown: Arc<AtomicBool>,
    progress_bytes: Arc<AtomicU64>,
    progress_callback: Option<Arc<dyn Fn(u64, usize, usize) + Send + Sync>>,
    completed: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
) -> DownloadResult {
    if shutdown.load(Ordering::SeqCst) {
        return DownloadResult::cancelled(recording.title.clone());
    }

    if recording.download_url.is_empty() {
        return DownloadResult::no_url(recording.title.clone());
    }

    // Build filename
    let filename = safe_filename(&format!("{}.mp4", recording.title));
    let out_path = out_dir.join(&filename);
    let tmp_path = out_dir.join(format!("{}.tmp", filename));

    // Try to download
    match try_download(
        &client,
        &token,
        &recording.download_url,
        &tmp_path,
        &shutdown,
        &progress_bytes,
        progress_callback,
        completed,
        failed,
    )
    .await
    {
        Ok(_) => {
            if shutdown.load(Ordering::SeqCst) {
                // Clean up partial download
                let _ = fs::remove_file(&tmp_path).await;
                return DownloadResult::cancelled(recording.title.clone());
            }

            // Move from temp to final location
            match fs::rename(&tmp_path, &out_path).await {
                Ok(_) => DownloadResult::ok(recording.title, out_path.to_string_lossy().to_string()),
                Err(e) => {
                    log::error!("Failed to rename temp file: {}", e);
                    let _ = fs::remove_file(&tmp_path).await;
                    DownloadResult::error(recording.title, format!("Rename failed: {}", e))
                }
            }
        }
        Err(e) => {
            // Clean up partial download
            let _ = fs::remove_file(&tmp_path).await;
            log::error!("Failed to download {}: {}", recording.title, e);
            DownloadResult::error(recording.title, e.to_string())
        }
    }
}

async fn try_download(
    client: &reqwest::Client,
    token: &str,
    url: &str,
    tmp_path: &Path,
    shutdown: &Arc<AtomicBool>,
    progress_bytes: &Arc<AtomicU64>,
    progress_callback: Option<Arc<dyn Fn(u64, usize, usize) + Send + Sync>>,
    completed: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
) -> Result<()> {
    // Try header authentication first
    let response = client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to send request")?;

    if response.status().is_success() {
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if content_type.starts_with("video")
            || content_type.starts_with("application/octet-stream")
            || content_type.starts_with("application/")
        {
            return write_stream(response, tmp_path, shutdown, progress_bytes, progress_callback, completed, failed).await;
        }
    }

    // Try query param fallback
    let sep = if url.contains('?') { "&" } else { "?" };
    let url_with_token = format!("{}{}access_token={}", url, sep, token);

    let response = client
        .get(&url_with_token)
        .send()
        .await
        .context("Failed to send request with token in URL")?;

    if response.status().is_success() {
        write_stream(response, tmp_path, shutdown, progress_bytes, progress_callback, completed, failed).await
    } else {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {}: {}", status, &text[..text.len().min(500)])
    }
}

async fn write_stream(
    response: reqwest::Response,
    out_path: &Path,
    shutdown: &Arc<AtomicBool>,
    progress_bytes: &Arc<AtomicU64>,
    progress_callback: Option<Arc<dyn Fn(u64, usize, usize) + Send + Sync>>,
    completed: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
) -> Result<()> {
    // Create parent directory if needed
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .await
            .context("Failed to create parent directory")?;
    }

    let mut file = File::create(out_path)
        .await
        .context("Failed to create output file")?;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if shutdown.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("Download interrupted"));
        }

        let chunk = chunk.context("Failed to read chunk")?;
        file.write_all(&chunk)
            .await
            .context("Failed to write chunk")?;

        progress_bytes.fetch_add(chunk.len() as u64, Ordering::SeqCst);

        if let Some(ref cb) = progress_callback {
            let bytes = progress_bytes.load(Ordering::SeqCst);
            let completed_val = completed.load(Ordering::SeqCst) as usize;
            let failed_val = failed.load(Ordering::SeqCst) as usize;
            cb(bytes, completed_val, failed_val);
        }
    }

    file.flush().await.context("Failed to flush file")?;
    Ok(())
}

fn safe_filename(s: &str) -> String {
    let keep = "-_.() abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    s.chars()
        .map(|c| if keep.contains(c) { c } else { '_' })
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}
