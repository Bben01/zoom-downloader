use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Days, NaiveDate, Utc};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const API_BASE: &str = "https://api.zoom.us/v2";
const OAUTH_TOKEN_URL: &str = "https://zoom.us/oauth/token";
/// Zoom only accepts date ranges of up to one month per list request.
const MAX_RANGE_DAYS: u64 = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub account_id: String,
    pub client_id: String,
    pub client_secret: String,
}

impl Credentials {
    pub fn is_complete(&self) -> bool {
        !self.account_id.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.client_secret.trim().is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Meeting {
    pub uuid: String,
    pub topic: String,
    pub start_time: DateTime<Utc>,
    pub duration_min: u32,
    /// Only completed MP4 files.
    pub files: Vec<RecordingFile>,
}

impl Meeting {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

#[derive(Clone, Debug)]
pub struct RecordingFile {
    pub id: String,
    pub recording_type: String,
    pub recording_start: DateTime<Utc>,
    pub download_url: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    meetings: Vec<ApiMeeting>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct ApiMeeting {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    topic: String,
    #[serde(default)]
    start_time: String,
    #[serde(default)]
    duration: u32,
    #[serde(default)]
    recording_files: Vec<ApiFile>,
}

#[derive(Deserialize)]
struct ApiFile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    file_type: String,
    #[serde(default)]
    file_extension: String,
    #[serde(default)]
    recording_type: String,
    #[serde(default)]
    recording_start: String,
    #[serde(default)]
    download_url: String,
    #[serde(default)]
    file_size: u64,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

pub struct ZoomClient {
    http: reqwest::Client,
    creds: Credentials,
    token: Mutex<Option<CachedToken>>,
}

impl ZoomClient {
    pub fn new(creds: Credentials) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("zoom-downloader/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(90))
            .build()
            .context("Could not create HTTP client")?;
        Ok(Self { http, creds, token: Mutex::new(None) })
    }

    pub fn credentials(&self) -> &Credentials {
        &self.creds
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Returns a valid access token, refreshing it shortly before it expires
    /// (Zoom tokens last one hour, long download runs outlive them).
    pub async fn token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        if let Some(t) = guard.as_ref() {
            if Instant::now() + Duration::from_secs(300) < t.expires_at {
                return Ok(t.value.clone());
            }
        }

        let resp = self
            .http
            .post(OAUTH_TOKEN_URL)
            .basic_auth(self.creds.client_id.trim(), Some(self.creds.client_secret.trim()))
            .query(&[
                ("grant_type", "account_credentials"),
                ("account_id", self.creds.account_id.trim()),
            ])
            .send()
            .await
            .context("Could not reach Zoom")?;

        if !resp.status().is_success() {
            return Err(api_error("Authentication failed", resp).await);
        }

        let body: TokenResponse = resp.json().await.context("Unexpected token response from Zoom")?;
        let ttl = Duration::from_secs(body.expires_in.unwrap_or(3600));
        let value = body.access_token;
        *guard = Some(CachedToken { value: value.clone(), expires_at: Instant::now() + ttl });
        Ok(value)
    }

    async fn invalidate_token(&self) {
        *self.token.lock().await = None;
    }

    /// Checks the credentials and that the app may list recordings for `user`.
    pub async fn verify(&self, user: &str) -> Result<()> {
        self.token().await?;
        let today = Utc::now().date_naive();
        self.list_page(&recordings_url(user), today, today, None).await?;
        Ok(())
    }

    /// Lists all meetings with at least one completed MP4 recording in `[from, to]`.
    pub async fn list_meetings(
        &self,
        user: &str,
        from: NaiveDate,
        to: NaiveDate,
        mut on_chunk: impl FnMut(NaiveDate, NaiveDate),
    ) -> Result<Vec<Meeting>> {
        if from > to {
            bail!("The start date is after the end date");
        }
        let url = recordings_url(user);
        let mut by_uuid: HashMap<String, Meeting> = HashMap::new();
        let mut chunk_start = from;

        while chunk_start <= to {
            let chunk_end = (chunk_start + Days::new(MAX_RANGE_DAYS - 1)).min(to);
            on_chunk(chunk_start, chunk_end);

            let mut page_token: Option<String> = None;
            loop {
                let page = self.list_page(&url, chunk_start, chunk_end, page_token.as_deref()).await?;
                for m in page.meetings {
                    if let Some(meeting) = convert_meeting(m) {
                        by_uuid.insert(meeting.uuid.clone(), meeting);
                    }
                }
                page_token = page.next_page_token.filter(|t| !t.is_empty());
                if page_token.is_none() {
                    break;
                }
            }
            chunk_start = chunk_end + Days::new(1);
        }

        let mut meetings: Vec<Meeting> = by_uuid.into_values().collect();
        meetings.sort_by(|a, b| a.start_time.cmp(&b.start_time).then_with(|| a.uuid.cmp(&b.uuid)));
        Ok(meetings)
    }

    async fn list_page(
        &self,
        url: &str,
        from: NaiveDate,
        to: NaiveDate,
        page_token: Option<&str>,
    ) -> Result<ListResponse> {
        let mut query = vec![
            ("from", from.to_string()),
            ("to", to.to_string()),
            ("page_size", "300".to_string()),
        ];
        if let Some(t) = page_token {
            query.push(("next_page_token", t.to_string()));
        }

        for attempt in 0..2 {
            let token = self.token().await?;
            let resp = self
                .http
                .get(url)
                .bearer_auth(&token)
                .query(&query)
                .send()
                .await
                .context("Could not reach Zoom")?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.invalidate_token().await;
                continue;
            }
            if !resp.status().is_success() {
                return Err(api_error("Listing recordings failed", resp).await);
            }
            return resp.json().await.context("Unexpected recordings response from Zoom");
        }
        unreachable!()
    }

    /// Moves every recording file of a meeting to the Zoom trash.
    pub async fn trash_meeting_recordings(&self, uuid: &str) -> Result<()> {
        let url = format!("{API_BASE}/meetings/{}/recordings", encode_uuid(uuid));
        for attempt in 0..2 {
            let token = self.token().await?;
            let resp = self
                .http
                .delete(&url)
                .bearer_auth(&token)
                .query(&[("action", "trash")])
                .send()
                .await
                .context("Could not reach Zoom")?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.invalidate_token().await;
                continue;
            }
            if !resp.status().is_success() {
                return Err(api_error("Moving to trash failed", resp).await);
            }
            return Ok(());
        }
        unreachable!()
    }
}

fn recordings_url(user: &str) -> String {
    let user = user.trim();
    let user = if user.is_empty() { "me" } else { user };
    format!("{API_BASE}/users/{}/recordings", utf8_percent_encode(user, NON_ALPHANUMERIC))
}

/// Zoom requires double encoding for UUIDs that start with `/` or contain `//`.
fn encode_uuid(uuid: &str) -> String {
    let once = utf8_percent_encode(uuid, NON_ALPHANUMERIC).to_string();
    if uuid.starts_with('/') || uuid.contains("//") {
        utf8_percent_encode(&once, NON_ALPHANUMERIC).to_string()
    } else {
        once
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

fn convert_meeting(m: ApiMeeting) -> Option<Meeting> {
    let start_time = parse_time(&m.start_time)?;
    let mut files: Vec<RecordingFile> = m
        .recording_files
        .into_iter()
        .filter(|f| {
            let is_mp4 = f.file_type.eq_ignore_ascii_case("mp4") || f.file_extension.eq_ignore_ascii_case("mp4");
            let completed = f.status.is_empty() || f.status.eq_ignore_ascii_case("completed");
            is_mp4 && completed && !f.download_url.is_empty() && !f.id.is_empty()
        })
        .map(|f| RecordingFile {
            recording_start: parse_time(&f.recording_start).unwrap_or(start_time),
            id: f.id,
            recording_type: f.recording_type,
            download_url: f.download_url,
            size: f.file_size,
        })
        .collect();
    files.sort_by(|a, b| a.recording_start.cmp(&b.recording_start).then_with(|| a.id.cmp(&b.id)));

    if files.is_empty() || m.uuid.is_empty() {
        return None;
    }
    let topic = m.topic.trim();
    Some(Meeting {
        uuid: m.uuid,
        topic: if topic.is_empty() { "Untitled meeting".into() } else { topic.into() },
        start_time,
        duration_min: m.duration,
        files,
    })
}

async fn api_error(context: &str, resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<ApiError>(&body)
        .ok()
        .and_then(|e| e.message.or(e.reason).or(e.error))
        .unwrap_or_else(|| body.chars().take(200).collect());
    let hint = match status.as_u16() {
        400 | 401 if context.starts_with("Authentication") => " — check the Account ID, Client ID and secret",
        401 | 403 => " — the Zoom app may be missing a required scope",
        404 => " — not found",
        429 => " — Zoom rate limit reached, try again in a minute",
        _ => "",
    };
    anyhow!("{context}: {} {detail}{hint}", status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_encoding() {
        assert_eq!(encode_uuid("abc123"), "abc123");
        assert_eq!(encode_uuid("ab+c=="), "ab%2Bc%3D%3D");
        assert_eq!(encode_uuid("/abc"), "%252Fabc");
        assert_eq!(encode_uuid("a//b"), "a%252F%252Fb");
    }
}
