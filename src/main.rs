mod downloader;
mod report;
mod zoom_api;

use chrono::{Duration, Utc};
use downloader::DownloadManager;
use report::{DownloadReport, DownloadStatus};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zoom_api::{ZoomClient, ZoomRecord};

slint::include_modules!();

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn update_ui<F>(app_weak: &Weak<AppWindow>, f: F)
where
    F: FnOnce(AppWindow) + Send + 'static,
{
    let app_weak = app_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            f(app);
        }
    })
    .ok();
}

#[tokio::main]
async fn main() -> Result<(), slint::PlatformError> {
    env_logger::init();

    let app = AppWindow::new()?;
    let app_weak = app.as_weak();

    // Initialize with default dates (last 7 days)
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let week_ago = (Utc::now() - Duration::days(7))
        .format("%Y-%m-%d")
        .to_string();
    app.set_start_date(week_ago.into());
    app.set_end_date(today.into());

    // Shared state for recordings and download manager
    let recordings = Arc::new(Mutex::new(Vec::<ZoomRecord>::new()));
    let download_manager = Arc::new(Mutex::new(None::<Arc<DownloadManager>>));

    // Fetch recordings callback
    {
        let app_weak = app_weak.clone();
        let recordings = recordings.clone();

        app.on_fetch_recordings(move || {
            let app_weak = app_weak.clone();
            let recordings = recordings.clone();

            // Get values from UI before spawning
            let (start_date, end_date) = {
                let app = match app_weak.upgrade() {
                    Some(app) => app,
                    None => return,
                };
                (
                    app.get_start_date().to_string(),
                    app.get_end_date().to_string(),
                )
            };

            update_ui(&app_weak, |app| {
                app.set_is_fetching(true);
                app.set_status_text("Fetching recordings...".into());
            });

            tokio::spawn(async move {
                let mut client = ZoomClient::new();

                match client.get_token().await {
                    Ok(_) => {
                        log::info!("Successfully authenticated with Zoom API");
                    }
                    Err(e) => {
                        let msg = format!("Authentication failed: {}", e);
                        log::error!("{}", msg);
                        let app_weak = app_weak.clone();
                        update_ui(&app_weak, move |app| {
                            app.set_status_text(msg.into());
                            app.set_is_fetching(false);
                        });
                        return;
                    }
                }

                match client
                    .list_recordings(&start_date, &end_date, Some("me"))
                    .await
                {
                    Ok(recs) => {
                        let mut rec_vec: Vec<ZoomRecord> = recs.into_iter().collect();
                        rec_vec.sort_by(|a, b| a.date.cmp(&b.date));
                        let count = rec_vec.len();

                        // Update UI with recordings
                        let recording_model: Vec<Recording> = rec_vec
                            .iter()
                            .map(|r| Recording {
                                title: r.title.clone().into(),
                                date: r.date.clone().into(),
                                size: format_size(r.file_size).into(),
                            })
                            .collect();

                        let app_weak_clone = app_weak.clone();
                        update_ui(&app_weak_clone, move |app| {
                            app.set_recordings(ModelRc::new(VecModel::from(recording_model)));
                            app.set_status_text(format!("Found {} recordings", count).into());
                        });

                        // Save recordings to shared state
                        *recordings.lock().unwrap() = rec_vec;
                    }
                    Err(e) => {
                        let msg = format!("Failed to fetch recordings: {}", e);
                        log::error!("{}", msg);
                        let app_weak = app_weak.clone();
                        update_ui(&app_weak, move |app| {
                            app.set_status_text(msg.into());
                        });
                    }
                }

                update_ui(&app_weak, |app| {
                    app.set_is_fetching(false);
                });
            });
        });
    }

    // Start download callback
    {
        let app_weak = app_weak.clone();
        let recordings = recordings.clone();
        let download_manager = download_manager.clone();

        app.on_start_download(move || {
            let app_weak = app_weak.clone();
            let recordings = recordings.clone();
            let download_manager = download_manager.clone();

            // Get values from UI before spawning
            let (output_dir, concurrency) = {
                let app = match app_weak.upgrade() {
                    Some(app) => app,
                    None => return,
                };
                (
                    app.get_output_dir().to_string(),
                    app.get_concurrency() as usize,
                )
            };

            update_ui(&app_weak, |app| {
                app.set_is_downloading(true);
                app.set_status_text("Starting downloads...".into());
            });

            tokio::spawn(async move {
                // Get OAuth token
                let mut client = ZoomClient::new();
                let token = match client.get_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        let msg = format!("Failed to get token: {}", e);
                        update_ui(&app_weak, move |app| {
                            app.set_status_text(msg.into());
                            app.set_is_downloading(false);
                        });
                        return;
                    }
                };

                // Load report and filter already downloaded
                let report_path = PathBuf::from(&output_dir).join("download_report.json");
                let mut report = DownloadReport::load(&report_path)
                    .await
                    .unwrap_or_else(|_| DownloadReport { results: Vec::new() });

                let previously_downloaded = report.get_successful_titles();
                let all_recordings = recordings.lock().unwrap().clone();
                let to_download: Vec<ZoomRecord> = all_recordings
                    .into_iter()
                    .filter(|r| !previously_downloaded.contains(&r.title))
                    .collect();

                if to_download.is_empty() {
                    update_ui(&app_weak, |app| {
                        app.set_status_text("All recordings already downloaded!".into());
                        app.set_is_downloading(false);
                    });
                    return;
                }

                let total_size: u64 = to_download.iter().map(|r| r.file_size).sum();
                let total_count = to_download.len();

                let status_msg = format!(
                    "Downloading {} files ({})...",
                    total_count,
                    format_size(total_size)
                );
                update_ui(&app_weak, move |app| {
                    app.set_status_text(status_msg.into());
                    app.set_stats(DownloadStats {
                        total: total_count as i32,
                        completed: 0,
                        failed: 0,
                        progress: 0.0,
                    });
                });

                // Create download manager
                let manager = Arc::new(DownloadManager::new(token, PathBuf::from(&output_dir), concurrency));
                manager.reset_progress();

                // Store manager for potential cancellation
                *download_manager.lock().unwrap() = Some(manager.clone());

                // Progress callback
                let app_weak_progress = app_weak.clone();
                let progress_callback: Arc<dyn Fn(u64, usize, usize) + Send + Sync> =
                    Arc::new(move |bytes, completed, failed| {
                        let progress = if total_size > 0 {
                            (bytes as f64 / total_size as f64) as f32
                        } else {
                            (completed as f64 / total_count as f64) as f32
                        };

                        update_ui(&app_weak_progress, move |app| {
                            app.set_stats(DownloadStats {
                                total: total_count as i32,
                                completed: completed as i32,
                                failed: failed as i32,
                                progress,
                            });
                        });
                    });

                // Download
                let results = manager
                    .download_batch(to_download, Some(progress_callback))
                    .await
                    .unwrap_or_default();

                // Update report
                for result in &results {
                    if result.status != DownloadStatus::Cancelled {
                        report.add_result(result.clone());
                    }
                }

                if let Err(e) = report.save(&report_path).await {
                    log::error!("Failed to save report: {}", e);
                }

                // Final stats
                let stats = report.get_stats(&results);
                let status = if manager.is_shutdown() {
                    format!(
                        "Download stopped. Success: {}, Errors: {}",
                        stats.success, stats.errors
                    )
                } else {
                    format!(
                        "Download complete! Success: {}, Errors: {}",
                        stats.success, stats.errors
                    )
                };

                update_ui(&app_weak, move |app| {
                    app.set_status_text(status.into());
                    app.set_is_downloading(false);
                });

                // Clear download manager
                *download_manager.lock().unwrap() = None;
            });
        });
    }

    // Stop download callback
    {
        let download_manager = download_manager.clone();
        let app_weak = app_weak.clone();

        app.on_stop_download(move || {
            let guard = download_manager.lock().unwrap();
            if let Some(manager) = guard.as_ref() {
                manager.signal_shutdown();
                update_ui(&app_weak, |app| {
                    app.set_status_text("Stopping downloads...".into());
                });
            }
        });
    }

    // File picker callback
    {
        let app_weak = app_weak.clone();
        app.on_request_dir_pick(move || {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                update_ui(&app_weak, move |app| {
                    app.set_output_dir(SharedString::from(path.display().to_string()));
                });
            }
        });
    }

    app.run()
}
