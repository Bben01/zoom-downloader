use crate::zoom::ZoomClient;
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, RANGE};
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

const MAX_ATTEMPTS: u32 = 4;

pub struct Job {
    pub meeting: usize,
    pub file_id: String,
    pub url: String,
    pub size: u64,
    pub dest: PathBuf,
}

pub enum Outcome {
    Downloaded,
    AlreadyPresent,
    Cancelled,
    Failed(String),
}

struct AttemptError {
    message: String,
    retry: bool,
}

impl AttemptError {
    fn retry(message: impl Into<String>) -> Self {
        Self { message: message.into(), retry: true }
    }
    fn fatal(message: impl Into<String>) -> Self {
        Self { message: message.into(), retry: false }
    }
}

/// Downloads one file to `job.dest`, resuming from a `.part` file when possible.
/// `file_bytes` tracks the bytes of this file on disk; `net_bytes` counts bytes received.
pub async fn download(
    client: &ZoomClient,
    job: &Job,
    file_bytes: &AtomicU64,
    net_bytes: &AtomicU64,
    cancel: &CancellationToken,
) -> Outcome {
    if let Ok(meta) = fs::metadata(&job.dest).await {
        if meta.is_file() && (job.size == 0 || meta.len() == job.size) {
            file_bytes.store(meta.len(), Ordering::Relaxed);
            return Outcome::AlreadyPresent;
        }
    }
    if let Some(parent) = job.dest.parent() {
        if let Err(e) = fs::create_dir_all(parent).await {
            return Outcome::Failed(format!("Cannot create folder: {e}"));
        }
    }

    let part = part_path(&job.dest);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match attempt_download(client, job, &part, file_bytes, net_bytes, cancel).await {
            Ok(()) => break,
            Err(_) if cancel.is_cancelled() => return Outcome::Cancelled,
            Err(e) if e.retry && attempt < MAX_ATTEMPTS => {
                log::warn!("Retrying {} (attempt {}/{}): {}", file_name(&job.dest), attempt + 1, MAX_ATTEMPTS, e.message);
                let backoff = Duration::from_secs(2u64.pow(attempt));
                tokio::select! {
                    _ = cancel.cancelled() => return Outcome::Cancelled,
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
            Err(e) => return Outcome::Failed(e.message),
        }
    }

    let written = match fs::metadata(&part).await {
        Ok(m) => m.len(),
        Err(e) => return Outcome::Failed(format!("Downloaded file vanished: {e}")),
    };
    if job.size > 0 && written != job.size {
        let _ = fs::remove_file(&part).await;
        return Outcome::Failed(format!("Incomplete download ({written} of {} bytes)", job.size));
    }
    if let Err(e) = fs::rename(&part, &job.dest).await {
        return Outcome::Failed(format!("Could not finalize file: {e}"));
    }
    Outcome::Downloaded
}

async fn attempt_download(
    client: &ZoomClient,
    job: &Job,
    part: &Path,
    file_bytes: &AtomicU64,
    net_bytes: &AtomicU64,
    cancel: &CancellationToken,
) -> Result<(), AttemptError> {
    let existing = fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    if job.size > 0 && existing == job.size {
        file_bytes.store(existing, Ordering::Relaxed);
        return Ok(());
    }
    if job.size > 0 && existing > job.size {
        let _ = fs::remove_file(part).await;
        return Err(AttemptError::retry("Partial file was larger than expected, restarting"));
    }

    let token = client.token().await.map_err(|e| AttemptError::retry(e.to_string()))?;
    let mut resp = send(client, &job.url, &token, existing, false).await?;
    // Some accounts only accept the token as a query parameter for downloads.
    if matches!(resp.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) || is_html(&resp) {
        resp = send(client, &job.url, &token, existing, true).await?;
    }

    let status = resp.status();
    let (file, start) = match status {
        StatusCode::PARTIAL_CONTENT if existing > 0 => {
            let f = OpenOptions::new().append(true).open(part).await;
            (f, existing)
        }
        StatusCode::OK => {
            let f = fs::File::create(part).await;
            (f, 0)
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let _ = fs::remove_file(part).await;
            return Err(AttemptError::retry("Server rejected resume, restarting"));
        }
        s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => {
            return Err(AttemptError::retry(format!("Zoom returned HTTP {}", s.as_u16())));
        }
        s => {
            return Err(AttemptError::fatal(match s.as_u16() {
                401 | 403 => format!("Access denied (HTTP {}) — check the app's recording scopes", s.as_u16()),
                404 => "Recording no longer exists on Zoom".to_string(),
                code => format!("Zoom returned HTTP {code}"),
            }));
        }
    };
    if is_html(&resp) {
        return Err(AttemptError::fatal("Zoom returned a web page instead of the video"));
    }

    let file = file.map_err(|e| AttemptError::fatal(format!("Cannot write file: {e}")))?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    file_bytes.store(start, Ordering::Relaxed);

    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = writer.flush().await;
                return Err(AttemptError::fatal("Cancelled"));
            }
            next = stream.next() => next,
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = writer.flush().await;
                return Err(AttemptError::retry(format!("Connection interrupted: {}", e.without_url())));
            }
        };
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| AttemptError::fatal(format!("Disk write failed: {e}")))?;
        file_bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        net_bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    writer
        .flush()
        .await
        .map_err(|e| AttemptError::fatal(format!("Disk write failed: {e}")))?;
    Ok(())
}

async fn send(
    client: &ZoomClient,
    url: &str,
    token: &str,
    resume_from: u64,
    token_in_query: bool,
) -> Result<reqwest::Response, AttemptError> {
    let mut req = if token_in_query {
        client.http().get(url).query(&[("access_token", token)])
    } else {
        client.http().get(url).bearer_auth(token)
    };
    if resume_from > 0 {
        req = req.header(RANGE, format!("bytes={resume_from}-"));
    }
    // The URL is stripped from errors because it may carry the access token.
    req.send()
        .await
        .map_err(|e| AttemptError::retry(format!("Network error: {}", e.without_url())))
}

fn is_html(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"))
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
