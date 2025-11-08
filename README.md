# Zoom Recording Downloader (Rust + Slint GUI)

A native Rust application with a GUI for bulk downloading Zoom cloud recordings.

## Features

- **Native GUI** built with Slint
- **Fast concurrent downloads** with configurable concurrency
- **Resume support** - tracks downloaded files and skips duplicates
- **Progress tracking** with real-time updates
- **OAuth 2.0 authentication** using Zoom Server-to-Server OAuth
- **Graceful shutdown** - can stop downloads mid-process
- **Download history** - JSON report of all downloads

## Prerequisites

- Rust 1.70+ (install from https://rustup.rs/)
- Zoom Server-to-Server OAuth app credentials

### Getting Zoom Credentials

1. Go to https://marketplace.zoom.us/
2. Create a Server-to-Server OAuth app
3. Note your Account ID, Client ID, and Client Secret
4. Add required scopes: `recording:read:admin` or `recording:read`

## Building

```bash
cargo build --release
```

The compiled binary will be in `target/release/zoom-downloader`

## Running

```bash
cargo run --release
```

Or run the compiled binary directly:

```bash
./target/release/zoom-downloader
```

## Usage

1. Launch the application
2. Enter your Zoom API credentials (Account ID, Client ID, Client Secret)
3. Set the date range for recordings (YYYY-MM-DD format)
4. Optionally adjust:
   - Output directory (default: `./zoom_downloads`)
   - Concurrency (default: 4 simultaneous downloads)
5. Click "Fetch Recordings" to retrieve the list
6. Click "Start Download" to begin downloading
7. Use "Stop Download" to gracefully cancel if needed

## Download Report

The application maintains a `download_report.json` file in the output directory that tracks:
- Successfully downloaded files
- Failed downloads with error messages
- Timestamps

On subsequent runs, the app automatically skips files that were already successfully downloaded.

## Architecture

- **src/main.rs** - GUI logic and event handling
- **src/zoom_api.rs** - Zoom API client
- **src/downloader.rs** - Concurrent download manager
- **src/report.rs** - Download tracking and reporting
- **ui/app.slint** - Slint GUI definition

## Python Reference

The original Python implementation is in `download.py` and was used as a reference for this Rust version.

## License

MIT
