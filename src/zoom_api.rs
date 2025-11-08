use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const API_BASE: &str = "https://api.zoom.us/v2";
const OAUTH_TOKEN_URL: &str = "https://zoom.us/oauth/token";

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct ZoomRecord {
    pub title: String,
    pub topic: String,
    pub date: String,
    pub download_url: String,
    pub file_size: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct RecordingsResponse {
    meetings: Vec<Meeting>,
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Meeting {
    topic: String,
    start_time: String,
    recording_files: Vec<RecordingFile>,
}

#[derive(Debug, Deserialize)]
struct RecordingFile {
    download_url: String,
    file_extension: String,
    file_size: Option<u64>,
}

pub struct ZoomClient {
    account_id: String,
    client_id: String,
    client_secret: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl ZoomClient {
    pub fn new(account_id: String, client_id: String, client_secret: String) -> Self {
        Self {
            account_id,
            client_id,
            client_secret,
            token: None,
            client: reqwest::Client::new(),
        }
    }

    pub async fn get_token(&mut self) -> Result<String> {
        if let Some(ref token) = self.token {
            return Ok(token.clone());
        }

        let auth = general_purpose::STANDARD
            .encode(format!("{}:{}", self.client_id, self.client_secret));

        let response = self
            .client
            .post(OAUTH_TOKEN_URL)
            .header("Authorization", format!("Basic {}", auth))
            .query(&[
                ("grant_type", "account_credentials"),
                ("account_id", &self.account_id),
            ])
            .send()
            .await
            .context("Failed to request OAuth token")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get token: {} - {}", status, text);
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse token response")?;

        self.token = Some(token_response.access_token.clone());
        Ok(token_response.access_token)
    }

    pub async fn list_recordings(
        &mut self,
        start_date: &str,
        end_date: &str,
        user_id: Option<&str>,
    ) -> Result<HashSet<ZoomRecord>> {
        let url = if let Some(user) = user_id {
            format!("{}/users/{}/recordings", API_BASE, user)
        } else {
            format!("{}/accounts/{}/recordings", API_BASE, self.account_id)
        };

        self.paginate_recordings(&url, start_date, end_date)
            .await
    }

    async fn paginate_recordings(
        &mut self,
        url: &str,
        start_date: &str,
        end_date: &str,
    ) -> Result<HashSet<ZoomRecord>> {
        // Parse dates
        let start = NaiveDate::parse_from_str(start_date, "%Y-%m-%d")
            .context("Invalid start date format")?;
        let end = NaiveDate::parse_from_str(end_date, "%Y-%m-%d")
            .context("Invalid end date format")?;

        let mut recordings = HashSet::new();
        let mut current_start = start;

        while current_start <= end {
            // Process in 30-day chunks (Zoom API limitation)
            let current_end = std::cmp::min(current_start + Duration::days(29), end);

            log::info!(
                "Fetching recordings from {} to {}",
                current_start,
                current_end
            );

            let chunk = self
                .fetch_recordings_page(url, &current_start.to_string(), &current_end.to_string())
                .await?;
            recordings.extend(chunk);

            // Move to next chunk
            current_start = current_end + Duration::days(1);
        }

        Ok(recordings)
    }

    async fn fetch_recordings_page(
        &mut self,
        url: &str,
        start_date: &str,
        end_date: &str,
    ) -> Result<HashSet<ZoomRecord>> {
        let token = self.get_token().await?;
        let mut recordings = HashSet::new();
        let mut next_token: Option<String> = None;

        loop {
            let mut params = vec![
                ("from", start_date.to_string()),
                ("to", end_date.to_string()),
                ("page_size", "300".to_string()),
            ];

            if let Some(ref token_val) = next_token {
                params.push(("next_page_token", token_val.clone()));
            }

            let response = self
                .client
                .get(url)
                .header("Authorization", format!("Bearer {}", token))
                .query(&params)
                .send()
                .await
                .context("Failed to fetch recordings")?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                anyhow::bail!("API request failed: {} - {}", status, text);
            }

            let recordings_response: RecordingsResponse = response
                .json()
                .await
                .context("Failed to parse recordings response")?;

            for meeting in recordings_response.meetings {
                let topic = meeting.topic.trim().to_string();
                let start_time = DateTime::parse_from_rfc3339(&meeting.start_time.replace("Z", "+00:00"))
                    .context("Failed to parse meeting start time")?;
                let date = start_time.format("%Y-%m-%d").to_string();
                let title = format!("{} - {}", topic, date);

                for file in meeting.recording_files {
                    if file.file_extension == "MP4" {
                        recordings.insert(ZoomRecord {
                            title: title.clone(),
                            topic: topic.clone(),
                            date: date.clone(),
                            download_url: file.download_url,
                            file_size: file.file_size.unwrap_or(0),
                        });
                    }
                }
            }

            next_token = recordings_response.next_page_token;
            if next_token.is_none() {
                break;
            }
        }

        Ok(recordings)
    }
}
