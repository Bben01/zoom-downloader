#!/usr/bin/env python3
"""
Zoom Bulk Download Tool (with meeting-level deletion)

Download Zoom cloud recordings in bulk with resume support.
After a successful MP4 download for a meeting, the script can delete the
entire meeting's cloud recordings (all files for the meeting) unless the
meeting is younger than --min-delete-age-days.

Requirements:
  pip install requests tqdm msgspec

Environment variables (preferred):
  ZOOM_ACCOUNT_ID      # account id (for S2S OAuth)
  ZOOM_CLIENT_ID
  ZOOM_CLIENT_SECRET

Usage:
  python download.py --from 2025-01-01 --to 2025-01-10 --out ./downloads --delete-after --min-delete-age-days 7
"""
import argparse
import base64
import json
import pathlib
import datetime
import logging
import os
import signal
import urllib.parse
from dataclasses import dataclass
from typing import Optional, Tuple, Set

import requests
from concurrent.futures import ThreadPoolExecutor, as_completed
from tqdm import tqdm
import msgspec

# Configure logging
logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(levelname)s - %(message)s',
    datefmt='%Y-%m-%d %H:%M:%S'
)
logger = logging.getLogger(__name__)


# ============================================================================
# Data Models
# ============================================================================

class ZoomRecord(msgspec.Struct):
    """Represents a single Zoom recording file (we filter MP4s)."""
    title: str
    topic: str
    date: str  # "YYYY-MM-DD"
    download_url: str
    file_size: int = 0
    meeting_id: str = ""     # meeting id or uuid (string)
    recording_id: str = ""   # file id

    def __hash__(self):
        return hash(self.download_url)

    def __eq__(self, other):
        return isinstance(other, ZoomRecord) and self.download_url == other.download_url


@dataclass
class DownloadResult:
    """Result of a download attempt."""
    status: str  # ok, skipped, error, exception, no_url, cancelled
    title: str
    path: Optional[str] = None
    error: Optional[str] = None
    timestamp: str = None
    meeting_id: Optional[str] = None
    recording_id: Optional[str] = None
    date: Optional[str] = None  # recording/meeting date YYYY-MM-DD
    delete_status: Optional[str] = None  # deleted_meeting, delete_failed:..., protected_by_min_age_...

    def __post_init__(self):
        if self.timestamp is None:
            self.timestamp = datetime.datetime.now(datetime.timezone.utc).isoformat()

    def to_dict(self):
        return {k: v for k, v in self.__dict__.items() if v is not None}


@dataclass
class DownloadReport:
    """Tracks download history across multiple runs."""
    results: list[dict]

    @classmethod
    def load(cls, report_path: pathlib.Path) -> "DownloadReport":
        """Load existing report or create new one."""
        if report_path.exists():
            try:
                with open(report_path, "r", encoding="utf8") as fh:
                    data = json.load(fh)
                    return cls(results=data.get("results", []))
            except Exception as e:
                logger.warning(f"Could not load report: {e}")
        return cls(results=[])

    def save(self, report_path: pathlib.Path):
        """Save report to disk."""
        report_path.parent.mkdir(parents=True, exist_ok=True)
        with open(report_path, "w", encoding="utf8") as fh:
            json.dump({"results": self.results}, fh, indent=2, ensure_ascii=False)

    def add_result(self, result: DownloadResult):
        """Add a download result to the report."""
        self.results.append(result.to_dict())

    def get_successful_titles(self) -> set[str]:
        """Get set of all successfully downloaded titles."""
        return {
            r["title"] for r in self.results
            if r.get("status") in ("ok", "skipped")
        }

    def get_stats(self, recent_results: list[DownloadResult]) -> dict:
        """Calculate download statistics."""
        return {
            "success": sum(1 for r in recent_results if r.status == "ok"),
            "skipped": sum(1 for r in recent_results if r.status == "skipped"),
            "errors": sum(1 for r in recent_results if r.status in ("error", "exception", "no_url")),
            "total_in_report": len(self.results)
        }


# ============================================================================
# Zoom API Client
# ============================================================================

class ZoomClient:
    """Client for interacting with Zoom API."""

    API_BASE = "https://api.zoom.us/v2"
    OAUTH_TOKEN_URL = "https://zoom.us/oauth/token"

    def __init__(self, account_id: str, client_id: str, client_secret: str):
        self.account_id = account_id
        self.client_id = client_id
        self.client_secret = client_secret
        self._token = None

    def get_token(self) -> str:
        """Get S2S OAuth token."""
        if self._token:
            return self._token

        auth = base64.b64encode(f"{self.client_id}:{self.client_secret}".encode()).decode()
        params = {"grant_type": "account_credentials", "account_id": self.account_id}
        headers = {"Authorization": f"Basic {auth}"}

        r = requests.post(self.OAUTH_TOKEN_URL, params=params, headers=headers, timeout=30)
        r.raise_for_status()

        self._token = r.json()["access_token"]
        return self._token

    def list_recordings(self, start_date: str, end_date: str, user_id: Optional[str] = None) -> Set[ZoomRecord]:
        """List all recordings in date range (returns MP4 files only)."""
        if user_id:
            url = f"{self.API_BASE}/users/{user_id}/recordings"
        else:
            url = f"{self.API_BASE}/accounts/{self.account_id}/recordings"

        return self._paginate_recordings(url, start_date, end_date)

    def _paginate_recordings(self, url: str, start_date: str, end_date: str) -> Set[ZoomRecord]:
        """Paginate through all recordings, splitting date ranges if necessary (30-day limit)."""
        start = datetime.datetime.strptime(start_date, "%Y-%m-%d").date()
        end = datetime.datetime.strptime(end_date, "%Y-%m-%d").date()

        recordings: Set[ZoomRecord] = set()
        current_start = start

        while current_start <= end:
            current_end = min(current_start + datetime.timedelta(days=29), end)
            logger.info(f"Fetching recordings from {current_start} to {current_end}")
            chunk = self._fetch_recordings_page(url, current_start.isoformat(), current_end.isoformat())
            recordings.update(chunk)
            current_start = current_end + datetime.timedelta(days=1)

        return recordings

    def _fetch_recordings_page(self, url: str, start_date: str, end_date: str) -> Set[ZoomRecord]:
        """Fetch recordings for a single date range (max 30 days)."""
        recordings: Set[ZoomRecord] = set()
        params = {"from": start_date, "to": end_date, "page_size": 300}
        headers = {"Authorization": f"Bearer {self.get_token()}"}
        next_token = ""

        while True:
            if next_token:
                params["next_page_token"] = next_token

            r = requests.get(url, params=params, headers=headers, timeout=30)
            r.raise_for_status()
            j = r.json()

            for meeting in j.get("meetings", []):
                topic = meeting.get("topic", "").strip()
                start_time = meeting.get("start_time")
                # parse to date (Zoom timestamps usually end with Z)
                date = datetime.datetime.fromisoformat(start_time.replace("Z", "+00:00")).date().isoformat()
                title = f"{topic} - {date}"
                meeting_id = str(meeting.get("uuid") or "")

                for f in meeting.get("recording_files", []):
                    if f.get("file_extension") == "MP4":
                        rec = ZoomRecord(
                            title=title,
                            topic=topic,
                            date=date,
                            download_url=f["download_url"],
                            file_size=f.get("file_size", 0),
                            meeting_id=meeting_id,
                            recording_id=str(f.get("id", ""))
                        )
                        recordings.add(rec)

            next_token = j.get("next_page_token") or ""
            if not next_token:
                break

        return recordings

    def delete_meeting_recordings(self, meeting_id: str) -> Tuple[bool, Optional[str]]:
        """Delete all recording files for a meeting.
        Calls DELETE /meetings/{meetingId}/recordings
        Returns (success, error_message).
        """
        if not meeting_id:
            return False, "missing meeting_id"

        q_meeting = urllib.parse.quote(str(meeting_id), safe='')
        q_meeting = urllib.parse.quote(str(q_meeting), safe='')
        url = f"{self.API_BASE}/meetings/{q_meeting}/recordings"
        headers = {"Authorization": f"Bearer {self.get_token()}"}

        try:
            r = requests.delete(url, headers=headers, timeout=30)
            if r.status_code in (200, 204):
                return True, None
            else:
                return False, f"HTTP {r.status_code}: {r.text[:500]}"
        except Exception as e:
            return False, str(e)


# ============================================================================
# Download Manager
# ============================================================================

class DownloadManager:
    """Manages concurrent downloads with progress tracking."""

    def __init__(self, token: str, out_dir: pathlib.Path, concurrency: int = 4):
        self.token = token
        self.out_dir = pathlib.Path(out_dir)
        self.concurrency = concurrency
        self.session = requests.Session()
        self._shutdown = False

        # Register signal handler for graceful shutdown
        signal.signal(signal.SIGINT, self._signal_handler)

    def _signal_handler(self, signum, frame):
        """Handle Ctrl+C gracefully."""
        logger.warning("\nInterrupt received, stopping downloads immediately...")
        self._shutdown = True

    def download_batch(self, recordings: list[ZoomRecord]) -> list[DownloadResult]:
        """Download a batch of recordings concurrently."""
        if not recordings:
            return []

        total_bytes = sum(rec.file_size for rec in recordings)
        results: list[DownloadResult] = []
        pbar = tqdm(total=total_bytes, desc="Downloading", unit="B", unit_scale=True, unit_divisor=1024)

        try:
            with ThreadPoolExecutor(max_workers=self.concurrency) as executor:
                futures = {executor.submit(self._download_one, rec, pbar): rec for rec in recordings}

                for future in as_completed(futures):
                    if self._shutdown:
                        # Cancel pending futures and stop
                        for f in futures:
                            f.cancel()
                        executor.shutdown(wait=False, cancel_futures=True)
                        break
                    result = future.result()
                    results.append(result)
        finally:
            pbar.close()

        return results

    def _download_one(self, rec: ZoomRecord, pbar: tqdm) -> DownloadResult:
        """Download a single recording file (MP4)."""
        tmp_path = None
        try:
            if self._shutdown:
                return DownloadResult(status="cancelled", title=rec.title,
                                      meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)

            if not rec.download_url:
                return DownloadResult(status="no_url", title=rec.title,
                                      meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)

            # Build filename
            fname = self._safe_filename(f"{rec.title}.mp4")
            out_path = self.out_dir / fname
            tmp_path = self.out_dir / f"{fname}.tmp"

            headers = {"Authorization": f"Bearer {self.token}"}

            # Try header authentication
            with self.session.get(rec.download_url, headers=headers, stream=True, timeout=60) as r:
                if r.status_code == 200 and r.headers.get("content-type", "").startswith(
                    ("video", "application/octet-stream", "application/")):
                    self._write_stream(r, tmp_path, pbar)
                    if self._shutdown:
                        if tmp_path.exists():
                            tmp_path.unlink()
                        return DownloadResult(status="cancelled", title=rec.title,
                                              meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)
                    tmp_path.rename(out_path)
                    return DownloadResult(status="ok", title=rec.title, path=str(out_path),
                                          meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)

            # Try query param fallback
            sep = "&" if "?" in rec.download_url else "?"
            url_with_token = f"{rec.download_url}{sep}access_token={self.token}"

            with self.session.get(url_with_token, stream=True, timeout=60) as r:
                if r.status_code == 200:
                    self._write_stream(r, tmp_path, pbar)
                    if self._shutdown:
                        if tmp_path.exists():
                            tmp_path.unlink()
                        return DownloadResult(status="cancelled", title=rec.title,
                                              meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)
                    tmp_path.rename(out_path)
                    return DownloadResult(status="ok", title=rec.title, path=str(out_path),
                                          meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)
                else:
                    error_msg = f"HTTP {r.status_code}: {r.text[:500]}"
                    logger.error(f"Failed to download {rec.title}: {error_msg}")
                    return DownloadResult(status="error", title=rec.title, error=error_msg,
                                          meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)

        except Exception as e:
            # Clean up partial download on exception
            if tmp_path and tmp_path.exists():
                try:
                    tmp_path.unlink()
                except Exception:
                    pass
            logger.error(f"Exception downloading {rec.title}: {e}")
            return DownloadResult(status="exception", title=rec.title, error=str(e),
                                  meeting_id=rec.meeting_id, recording_id=rec.recording_id, date=rec.date)

    def _write_stream(self, resp, out_path: pathlib.Path, pbar: tqdm):
        """Write HTTP response stream to file with byte-level progress tracking."""
        out_path.parent.mkdir(parents=True, exist_ok=True)

        with open(out_path, "wb") as f:
            for chunk in resp.iter_content(chunk_size=1 << 20):  # 1MB
                if self._shutdown:
                    raise InterruptedError("Download interrupted")
                if chunk:
                    f.write(chunk)
                    pbar.update(len(chunk))

    @staticmethod
    def _safe_filename(s: str) -> str:
        """Convert string to safe filename."""
        keep = "-_.() abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        return "".join(c if c in keep else "_" for c in s)[:200].strip()


# ============================================================================
# Main Application
# ============================================================================

def parse_args():
    parser = argparse.ArgumentParser(
        description="Bulk download Zoom cloud recordings",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )

    parser.add_argument("--from", "-f", dest="start", help="Start date (YYYY-MM-DD)")
    parser.add_argument("--to", "-t", dest="end", help="End date (YYYY-MM-DD)")
    parser.add_argument("--out", "-o", default="./zoom_downloads", help="Output directory (default: ./zoom_downloads)")
    parser.add_argument("--concurrency", "-c", type=int, default=4, help="Concurrent downloads (default: 4)")
    parser.add_argument("--user", "-u", dest="user_id", default="me", help="User email or 'me' for user-level listing")
    parser.add_argument("--days", "-d", type=int, default=7, help="Number of days to look back if no dates specified (default: 7)")
    parser.add_argument("--delete-after", "-D", action="store_true", help="Delete entire meeting recordings after successful MP4 download")
    parser.add_argument("--min-delete-age-days", "-K", type=int, default=10,
                        help="Do not delete meetings younger than this many days (default: 10)")

    return parser.parse_args()


def main():
    args = parse_args()

    # Load credentials: env vars first, then zoom-credentials.json next to this script
    file_creds = {}
    creds_path = pathlib.Path(__file__).resolve().parent / "zoom-credentials.json"
    if creds_path.is_file():
        file_creds = json.loads(creds_path.read_text())
    account_id = os.getenv("ZOOM_ACCOUNT_ID") or file_creds.get("account_id")
    client_id = os.getenv("ZOOM_CLIENT_ID") or file_creds.get("client_id")
    client_secret = os.getenv("ZOOM_CLIENT_SECRET") or file_creds.get("client_secret")

    if not all([account_id, client_id, client_secret]):
        logger.error("Error: Set ZOOM_ACCOUNT_ID, ZOOM_CLIENT_ID, ZOOM_CLIENT_SECRET or create zoom-credentials.json")
        return 1

    # Set date range
    if not args.start:
        args.start = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=args.days)).strftime("%Y-%m-%d")
    if not args.end:
        args.end = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")

    logger.info("Zoom Download Tool")
    logger.info(f"Date range: {args.start} to {args.end}")
    logger.info(f"Output: {args.out}")
    if args.delete_after:
        logger.info("DELETE AFTER DOWNLOAD: enabled (will attempt to delete entire meeting recordings after successful MP4 download)")
    if args.min_delete_age_days:
        logger.info(f"Will protect meetings younger than {args.min_delete_age_days} days from deletion")

    # Initialize components
    client = ZoomClient(account_id, client_id, client_secret)
    report_path = pathlib.Path(args.out) / "download_report.json"
    report = DownloadReport.load(report_path)

    # Get OAuth token
    logger.info("Authenticating...")
    client.get_token()

    # List recordings
    logger.info("Fetching recordings list...")
    user_id = args.user_id or "me"
    recordings = client.list_recordings(args.start, args.end, user_id=user_id)
    logger.info(f"Found {len(recordings)} recordings")

    # Filter out already downloaded
    previously_downloaded = report.get_successful_titles()
    to_download = [r for r in recordings if r.title not in previously_downloaded]

    if len(to_download) < len(recordings):
        skipped_count = len(recordings) - len(to_download)
        logger.info(f"Skipping {skipped_count} already downloaded recordings")

    if not to_download:
        logger.info("Nothing new to download!")
        return 0

    logger.info(f"Downloading {len(to_download)} new recordings...")

    manager = DownloadManager(client.get_token(), args.out, args.concurrency)
    results = manager.download_batch(to_download)

    # After a successful MP4 download, attempt to delete the entire meeting (unless protected by age)
    now_date = datetime.datetime.now(datetime.timezone.utc).date()
    min_days = args.min_delete_age_days or 0

    for result in results:
        if result.status != "ok":
            continue

        if not args.delete_after:
            continue

        # Determine protection by meeting date (safe fallback: protect when parsing fails)
        protected = False
        if result.date:
            try:
                rec_date = datetime.date.fromisoformat(result.date)
                age_days = (now_date - rec_date).days
                if age_days < min_days:
                    result.delete_status = f"protected_by_min_age_{min_days}d"
                    protected = True
            except Exception:
                # conservative: protect if we can't parse the date
                result.delete_status = "protected_by_min_age_parse_error"
                protected = True
        else:
            result.delete_status = "protected_no_date"
            protected = True

        if protected:
            logger.info(f"Skipping deletion for protected meeting: {result.title} ({result.meeting_id})")
            continue

        # Attempt to delete all recording files for the meeting
        success, err = client.delete_meeting_recordings(result.meeting_id or "")
        if success:
            result.delete_status = "deleted_meeting"
            logger.info(f"Deleted meeting recordings: {result.title} ({result.meeting_id})")
        else:
            result.delete_status = f"delete_failed: {err}"
            logger.warning(f"Failed to delete meeting {result.title} ({result.meeting_id}): {err}")

    # Update report only with non-cancelled downloads
    for result in results:
        if result.status != "cancelled":
            report.add_result(result)
    report.save(report_path)

    stats = report.get_stats(results)
    logger.info("=" * 60)
    logger.info("Download Summary:")
    logger.info(f"  Success: {stats['success']}")
    logger.info(f"  Errors: {stats['errors']}")
    logger.info(f"  Total in history: {stats['total_in_report']}")
    logger.info(f"  Report: {report_path}")
    logger.info("=" * 60)

    return 0 if stats['errors'] == 0 else 1


if __name__ == "__main__":
    exit(main())
