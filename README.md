# Zoom Downloader

A desktop app for Windows (it also runs on macOS and Linux) that bulk-downloads Zoom cloud recordings. It can also move each meeting to the Zoom trash once all of its videos are safely on disk.

## Features

- **Dark UI** with a sidebar and three pages: Recordings, Settings and Activity
- **Per-meeting view** showing date, duration, size, a status badge and live progress
- **Fast, resumable downloads**: several files download at once, and interrupted files continue from where they stopped (`.part` files + HTTP range requests). Failed network requests are retried automatically.
- **Unique file names**: meetings with several videos no longer overwrite each other
- **Safe cloud cleanup** (optional): a meeting is moved to the Zoom *trash* (recoverable for 30 days) only when **every** MP4 of that meeting downloaded successfully and the meeting is older than the "keep recent" threshold. You confirm before the run starts.
- **Secure credentials**: the client secret is kept in the Windows Credential Manager (Keychain on macOS), not in a plain text file
- **Download history** (`zoom-downloader-history.json`) in the output folder, so files are never downloaded twice
- **Activity log** in the app, plus a log file on disk for troubleshooting

## Install (Windows)

1. Open the repository's [Releases](../../releases) page.
2. Download `ZoomDownloader-<version>-windows-x64.zip` (or the standalone `.exe`).
3. Unzip it and run **Zoom Downloader.exe**. Nothing needs to be installed. If Windows SmartScreen warns about an unknown publisher, click *More info → Run anyway*.

## Zoom credentials

The app needs three keys (Account ID, Client ID, Client secret) from a "Server-to-Server OAuth" app in your Zoom account. It takes about 5 minutes and is done once. The same steps are shown in the app under *Settings → How do I get these keys?*.

You need a paid Zoom plan with cloud recording, and an account role that can add admin-level scopes (usually admin or owner). If you can't select the admin scopes in step 5, ask your Zoom account administrator to grant the permission or to create the app for you.

1. Open <https://marketplace.zoom.us/develop/create> and sign in with the Zoom account that owns the recordings.
2. Choose **Server-to-Server OAuth app** (if the picker isn't shown, click **Develop** in the lower-left corner, then **+** next to *Developer* → **Build an app**), give it any name (for example "Recording downloader") and click **Create**.
3. On **App credentials**, copy the **Account ID**, **Client ID** and **Client secret** into *Settings*.
4. On **Information**, fill in the required fields (short description, company name, developer name and email).
5. On **Scopes**, click **Add scopes** and add:
   - `cloud_recording:read:list_user_recordings:admin` — list recordings and download them (classic name: `recording:read:admin`).
   - For cloud cleanup only: `cloud_recording:delete:meeting_recording:admin` — move a meeting's recordings to the Zoom trash (classic name: `recording:write:admin`).
6. On **Activation**, click **Activate your app**. It can't issue tokens until it is activated.
7. For cloud cleanup only: in the Zoom web portal, open **Settings → Recording** and make sure cloud recording and **The host can delete cloud recordings** are enabled.
8. In the app, press **Test connection**.

If the test says the token "does not contain scopes", go back to step 5. If the client secret is lost, click **Regenerate** on the App credentials page. Never share the client secret. Recordings moved to the Zoom trash can be recovered from the Zoom web portal for up to 30 days.

Instead of typing the keys, you can put a `zoom-credentials.json` file next to the app; it fills any empty credential fields at launch:

```json
{ "account_id": "...", "client_id": "...", "client_secret": "..." }
```

The `ZOOM_ACCOUNT_ID`, `ZOOM_CLIENT_ID` and `ZOOM_CLIENT_SECRET` environment variables are used as a last fallback.

## Usage

1. **Recordings** → choose a date range (or a preset) → **Fetch recordings**.
2. Pick the meetings you want. Meetings that are already on disk start unticked.
3. Press **Download**. You can press **Stop** at any time; the next run picks up where it left off.

Files are saved as `Topic - YYYY-MM-DD HHhMM.mp4`, using the recording's start time in local time.

## Where things are stored (Windows)

| What | Location |
| --- | --- |
| Settings | `%APPDATA%\ZoomDownloader\Zoom Downloader\config\settings.json` |
| Client secret | Windows Credential Manager, entry `zoom-downloader` |
| Log file | `%LOCALAPPDATA%\ZoomDownloader\Zoom Downloader\data\logs\zoom-downloader.log` |
| Download history | `<output folder>\zoom-downloader-history.json` |

## Building from source

```bash
cargo run            # debug
cargo build --release
cargo test
```

Release builds on Windows statically link the C runtime (`.cargo/config.toml`) and embed the icon (`assets/icon.ico`). If the window doesn't appear on a machine without GPU drivers, try setting `SLINT_BACKEND=winit-software`.

## Releasing

The [Build & Release workflow](.github/workflows/release.yml) builds the Windows app on every push and pull request, and uploads the `.exe` and `.zip` as workflow artifacts.

To publish a release, bump `version` in `Cargo.toml` and push to `main`. When no `v<version>` tag exists yet, the workflow creates that tag and a GitHub release with the Windows binaries attached.

## Project layout

- `src/main.rs`: startup, runtime and fatal error dialog
- `src/app.rs`: UI controller (fetching, selection, the download run, cleanup)
- `src/zoom.rs`: Zoom API client (OAuth token refresh, listing, trashing)
- `src/download.rs`: resumable file downloader with retries
- `src/naming.rs`: unique, Windows-safe file names
- `src/history.rs`, `src/settings.rs`, `src/logger.rs`: persistence and logging
- `ui/`: Slint UI (`app.slint`, `components.slint`, `globals.slint`, icons)
- `download.py`: the original Python script

## License

MIT
