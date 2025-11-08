#!/usr/bin/env python3
"""
Zoom Bulk Download Tool

Download Zoom cloud recordings in bulk with resume support.

Requirements:
  pip install requests tqdm msgspec

Environment variables (preferred):
  ZOOM_ACCOUNT_ID      # account id (for S2S OAuth)
  ZOOM_CLIENT_ID
  ZOOM_CLIENT_SECRET

Usage:
  python download.py --from 2025-01-01 --to 2025-01-10 --out ./downloads
"""
import argparse
import base64
import json
import pathlib
import datetime
import logging
import signal
import sys
from dataclasses import dataclass
from typing import Optional

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
    """Represents a single Zoom recording."""
    title: str
    topic: str
    date: str
    download_url: str
    file_size: int = 0

    def __hash__(self):
        return hash(self.download_url)

    def __eq__(self, other):
        return isinstance(other, ZoomRecord) and self.download_url == other.download_url


@dataclass
class DownloadResult:
    """Result of a download attempt."""
    status: str  # ok, skipped, error, exception, no_url
    title: str
    path: Optional[str] = None
    error: Optional[str] = None
    timestamp: str = None

    def __post_init__(self):
        if self.timestamp is None:
            self.timestamp = datetime.datetime.now(datetime.UTC).isoformat()

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

    def list_recordings(self, start_date: str, end_date: str, user_id: Optional[str] = None) -> set[ZoomRecord]:
        """List all recordings in date range."""
        if user_id:
            url = f"{self.API_BASE}/users/{user_id}/recordings"
        else:
            url = f"{self.API_BASE}/accounts/{self.account_id}/recordings"

        return self._paginate_recordings(url, start_date, end_date)

    def _paginate_recordings(self, url: str, start_date: str, end_date: str) -> set[ZoomRecord]:
        """Paginate through all recordings, splitting date ranges if necessary."""
        # Zoom API limits to 30 days per request, so split if needed
        start = datetime.datetime.strptime(start_date, "%Y-%m-%d").date()
        end = datetime.datetime.strptime(end_date, "%Y-%m-%d").date()

        recordings = set()
        current_start = start

        while current_start <= end:
            # Process in 30-day chunks
            current_end = min(current_start + datetime.timedelta(days=29), end)

            logger.info(f"Fetching recordings from {current_start} to {current_end}")
            chunk_recordings = self._fetch_recordings_page(
                url,
                current_start.isoformat(),
                current_end.isoformat()
            )
            recordings.update(chunk_recordings)

            # Move to next chunk
            current_start = current_end + datetime.timedelta(days=1)

        return recordings

    def _fetch_recordings_page(self, url: str, start_date: str, end_date: str) -> set[ZoomRecord]:
        """Fetch recordings for a single date range (max 30 days)."""
        recordings = set()
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
                date = datetime.datetime.fromisoformat(start_time.replace("Z", "+00:00")).date().isoformat()
                title = f"{topic} - {date}"

                for f in meeting.get("recording_files", []):
                    if f.get("file_extension") == "MP4":
                        recordings.add(ZoomRecord(
                            title=title,
                            topic=topic,
                            date=date,
                            download_url=f["download_url"],
                            file_size=f.get("file_size", 0)
                        ))

            next_token = j.get("next_page_token") or ""
            if not next_token:
                break

        return recordings


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

        # Calculate total size from file_size field
        total_bytes = sum(rec.file_size for rec in recordings)

        results = []
        pbar = tqdm(total=total_bytes, desc="Downloading", unit="B", unit_scale=True, unit_divisor=1024)

        try:
            with ThreadPoolExecutor(max_workers=self.concurrency) as executor:
                futures = {executor.submit(self._download_one, rec, pbar): rec for rec in recordings}

                for future in as_completed(futures):
                    if self._shutdown:
                        # Cancel all pending futures immediately
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
        """Download a single recording."""
        tmp_path = None
        try:
            if self._shutdown:
                return DownloadResult(status="cancelled", title=rec.title)

            if not rec.download_url:
                return DownloadResult(status="no_url", title=rec.title)

            # Build filename
            fname = self._safe_filename(f"{rec.title}.mp4")
            out_path = self.out_dir / fname
            tmp_path = self.out_dir / f"{fname}.tmp"

            # Download to temp file first
            headers = {"Authorization": f"Bearer {self.token}"}

            # Try header authentication first
            with self.session.get(rec.download_url, headers=headers, stream=True, timeout=60) as r:
                if r.status_code == 200 and r.headers.get("content-type", "").startswith(
                    ("video", "application/octet-stream", "application/")):
                    self._write_stream(r, tmp_path, pbar)
                    if self._shutdown:
                        # Clean up partial download
                        if tmp_path.exists():
                            tmp_path.unlink()
                        return DownloadResult(status="cancelled", title=rec.title)
                    # Move from temp to final location
                    tmp_path.rename(out_path)
                    return DownloadResult(status="ok", title=rec.title, path=str(out_path))

            # Try query param fallback
            sep = "&" if "?" in rec.download_url else "?"
            url_with_token = f"{rec.download_url}{sep}access_token={self.token}"

            with self.session.get(url_with_token, stream=True, timeout=60) as r:
                if r.status_code == 200:
                    self._write_stream(r, tmp_path, pbar)
                    if self._shutdown:
                        # Clean up partial download
                        if tmp_path.exists():
                            tmp_path.unlink()
                        return DownloadResult(status="cancelled", title=rec.title)
                    # Move from temp to final location
                    tmp_path.rename(out_path)
                    return DownloadResult(status="ok", title=rec.title, path=str(out_path))
                else:
                    error_msg = f"HTTP {r.status_code}: {r.text[:500]}"
                    logger.error(f"Failed to download {rec.title}: {error_msg}")
                    return DownloadResult(status="error", title=rec.title, error=error_msg)

        except Exception as e:
            # Clean up partial download on exception
            if tmp_path and tmp_path.exists():
                try:
                    tmp_path.unlink()
                except Exception:
                    pass
            logger.error(f"Exception downloading {rec.title}: {e}")
            return DownloadResult(status="exception", title=rec.title, error=str(e))

    def _write_stream(self, resp, out_path: pathlib.Path, pbar: tqdm):
        """Write HTTP response stream to file with byte-level progress tracking."""
        out_path.parent.mkdir(parents=True, exist_ok=True)

        with open(out_path, "wb") as f:
            for chunk in resp.iter_content(chunk_size=1 << 20):  # 1MB chunks
                if self._shutdown:
                    # Stop writing immediately on interrupt
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
    """Parse command line arguments."""
    parser = argparse.ArgumentParser(
        description="Bulk download Zoom cloud recordings",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Download last 7 days
  python download.py --from 2025-01-01 --to 2025-01-07

  # Download with custom output directory and concurrency
  python download.py --out ~/zoom_backups --concurrency 8

  # Download for specific user
  python download.py --user me
        """
    )

    parser.add_argument("--from", "-f", dest="start", help="Start date (YYYY-MM-DD)")
    parser.add_argument("--to", "-t", dest="end", help="End date (YYYY-MM-DD)")
    parser.add_argument("--out", "-o", default="./zoom_downloads", help="Output directory (default: ./zoom_downloads)")
    parser.add_argument("--concurrency", "-c", type=int, default=4, help="Concurrent downloads (default: 4)")
    parser.add_argument("--user", "-u", dest="user_id", default="me", help="User email or 'me' for user-level listing")
    parser.add_argument("--days", "-d", type=int, default=7, help="Number of days to look back if no dates specified (default: 7)")

    return parser.parse_args()


def main():
    """Main application entry point."""
    args = parse_args()

    # Load credentials
    account_id = "***REMOVED***"  # TODO: os.getenv("ZOOM_ACCOUNT_ID")
    client_id = "***REMOVED***"  # TODO: os.getenv("ZOOM_CLIENT_ID")
    client_secret = "***REMOVED***"  # TODO: os.getenv("ZOOM_CLIENT_SECRET")

    if not all([account_id, client_id, client_secret]):
        logger.error("Error: Set ZOOM_ACCOUNT_ID, ZOOM_CLIENT_ID, ZOOM_CLIENT_SECRET")
        return 1

    # Set date range
    if not args.start:
        args.start = (datetime.datetime.now(datetime.UTC) - datetime.timedelta(days=args.days)).strftime("%Y-%m-%d")
    if not args.end:
        args.end = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%d")

    logger.info(f"Zoom Download Tool")
    logger.info(f"Date range: {args.start} to {args.end}")
    logger.info(f"Output: {args.out}")

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

    # Download
    manager = DownloadManager(client.get_token(), args.out, args.concurrency)
    results = manager.download_batch(to_download)

    # Update report only with successful downloads (exclude cancelled ones)
    for result in results:
        if result.status != "cancelled":
            report.add_result(result)
    report.save(report_path)

    # Print summary
    stats = report.get_stats(results)
    logger.info("=" * 60)
    logger.info(f"Download Summary:")
    logger.info(f"  Success: {stats['success']}")
    logger.info(f"  Errors: {stats['errors']}")
    logger.info(f"  Total in history: {stats['total_in_report']}")
    logger.info(f"  Report: {report_path}")
    logger.info("=" * 60)

    return 0 if stats['errors'] == 0 else 1


if __name__ == "__main__":
    exit(main())
