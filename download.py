#!/usr/bin/env python3
"""
zoom_bulk_download.py

Requirements:
  pip install requests tqdm

Environment variables (preferred):
  ZOOM_ACCOUNT_ID      # account id (for S2S OAuth)
  ZOOM_CLIENT_ID
  ZOOM_CLIENT_SECRET
Optional:
  ZOOM_USER_ID         # if you want user-level listing (email or 'me'), otherwise account-level used
Usage:
  python zoom_bulk_download.py --from 2025-01-01 --to 2025-10-09 --out ./downloads --concurrency 6
"""
import os
import sys
import argparse
import base64
import json
import pathlib
import datetime
import requests
from concurrent.futures import ThreadPoolExecutor, as_completed
from tqdm import tqdm

import msgspec


# Define msgspec struct
class ZoomRecord(msgspec.Struct):
    title: str
    topic: str
    date: str
    download_url: str

    def __hash__(self):
        return hash(self.download_url)

    def __eq__(self, other):
        return isinstance(other, ZoomRecord) and self.download_url == other.download_url


API_BASE = "https://api.zoom.us/v2"
OAUTH_TOKEN_URL = "https://zoom.us/oauth/token"


def get_s2s_token(account_id, client_id, client_secret):
    auth = base64.b64encode(f"{client_id}:{client_secret}".encode()).decode()
    params = {"grant_type": "account_credentials", "account_id": account_id}
    headers = {"Authorization": f"Basic {auth}"}
    r = requests.post(OAUTH_TOKEN_URL, params=params, headers=headers, timeout=30)
    r.raise_for_status()
    return r.json()["access_token"]


def list_account_recordings(access_token, account_id, start, end):
    url = f"{API_BASE}/accounts/{account_id}/recordings"
    return _paginate_recordings(url, access_token, start, end)


def list_user_recordings(access_token, user_id, start, end):
    url = f"{API_BASE}/users/{user_id}/recordings"
    return _paginate_recordings(url, access_token, start, end)


def _paginate_recordings(url, access_token, start, end) -> set[ZoomRecord]:
    recordings = set()
    params = {"from": start, "to": end, "page_size": 300}
    headers = {"Authorization": f"Bearer {access_token}"}
    next_token = ""
    while True:
        if next_token:
            params["next_page_token"] = next_token
        r = requests.get(url, params=params, headers=headers, timeout=30)
        r.raise_for_status()
        j = r.json()
        # different endpoints may return 'meetings' or 'recordings' objects; try both
        for meeting in j["meetings"]:
            topic = meeting.get("topic", "").strip()
            start_time = meeting.get("start_time")
            date = datetime.datetime.fromisoformat(start_time.replace("Z", "+00:00")).date().isoformat()
            title = f"{topic} - {date}"
            for f in meeting.get("recording_files", []):
                if f.get("file_extension") == "MP4":
                    recordings.add(
                        ZoomRecord(
                            title=title,
                            topic=topic,
                            date=date,
                            download_url=f["download_url"]
                        )
                    )
        next_token = j.get("next_page_token") or ""
        if not next_token:
            break
    # dedupe by file id if appears multiple times
    return recordings


def safe_filename(s):
    keep = "-_.() abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
    return "".join(c if c in keep else "_" for c in s)[:200].strip()


def download_one(rec: ZoomRecord, access_token, out_dir, session=None):
    """Download a single Zoom recording. Returns result dict with status."""
    session = session or requests.Session()
    download_url = rec.download_url
    if not download_url:
        return {"status": "no_url", "title": rec.title}

    # build filename
    fname = f"{rec.title}.mp4"
    fname = safe_filename(fname)
    out_path = pathlib.Path(out_dir) / fname

    headers = {"Authorization": f"Bearer {access_token}"}
    try:
        # try header first
        with session.get(download_url, headers=headers, stream=True, timeout=60) as r:
            if r.status_code == 200 and r.headers.get("content-type", "").startswith(
                    ("video", "application/octet-stream", "application/")):
                _stream_write(r, out_path)
                return {"status": "ok", "path": str(out_path), "title": rec.title}
        # try query param fallback
        sep = "&" if "?" in download_url else "?"
        url_with_token = f"{download_url}{sep}access_token={access_token}"
        with session.get(url_with_token, stream=True, timeout=60) as r2:
            if r2.status_code == 200:
                _stream_write(r2, out_path)
                return {"status": "ok", "path": str(out_path), "title": rec.title}
            else:
                return {"status": "error", "code": r2.status_code, "reason": r2.text[:500], "title": rec.title}
    except Exception as e:
        return {"status": "exception", "error": str(e), "title": rec.title}


def _stream_write(resp, out_path):
    """Write response stream to file without progress bar (to avoid tqdm conflicts)."""
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with open(out_path, "wb") as f:
        for chunk in resp.iter_content(chunk_size=1 << 20):
            if chunk:
                f.write(chunk)


def load_previous_downloads(out_dir):
    """Load previously downloaded files from report to avoid re-downloading."""
    report_path = pathlib.Path(out_dir) / "download_report.json"
    downloaded_titles = set()
    if report_path.exists():
        try:
            with open(report_path, "r", encoding="utf8") as fh:
                data = json.load(fh)
                for result in data.get("results", []):
                    if result.get("status") in ("ok", "skipped"):
                        downloaded_titles.add(result.get("title"))
        except Exception as e:
            print(f"Warning: Could not load previous download report: {e}")
    return downloaded_titles


def main():
    p = argparse.ArgumentParser(description="Bulk download Zoom recordings")
    p.add_argument("--from", dest="start", required=False, help="Start date (YYYY-MM-DD)")
    p.add_argument("--to", dest="end", required=False, help="End date (YYYY-MM-DD)")
    p.add_argument("--out", default="./zoom_downloads", help="Output directory")
    p.add_argument("--concurrency", type=int, default=4, help="Number of concurrent downloads")
    p.add_argument("--user", dest="user_id", default=None, help="User email or 'me' for user-level listing")
    args = p.parse_args()

    account_id = "***REMOVED***"  # os.getenv("ZOOM_ACCOUNT_ID")
    client_id = "***REMOVED***"  # os.getenv("ZOOM_CLIENT_ID")
    client_secret = "***REMOVED***"  # os.getenv("ZOOM_CLIENT_SECRET")
    if not (account_id and client_id and client_secret):
        print("Set ZOOM_ACCOUNT_ID, ZOOM_CLIENT_ID, ZOOM_CLIENT_SECRET in environment.", file=sys.stderr)
        sys.exit(2)

    # default dates: last 3 days
    if not args.start:
        args.start = (datetime.datetime.now(datetime.UTC) - datetime.timedelta(days=3)).strftime("%Y-%m-%d")
    if not args.end:
        args.end = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%d")

    print("Getting S2S token...")
    token = get_s2s_token(account_id, client_id, client_secret)

    print("Listing recordings...")
    args.user_id = args.user_id or "me"
    if args.user_id:
        recs = list_user_recordings(token, args.user_id, args.start, args.end)
    else:
        recs = list_account_recordings(token, account_id, args.start, args.end)
    print(f"Found {len(recs)} recording files.")

    # Load previously downloaded files
    previously_downloaded = load_previous_downloads(args.out)
    recs_to_download = [r for r in recs if r.title not in previously_downloaded]

    if len(recs_to_download) < len(recs):
        print(f"Skipping {len(recs) - len(recs_to_download)} already downloaded files.")
    print(f"Downloading {len(recs_to_download)} files...")

    # Download with single progress bar
    results = []
    session = requests.Session()
    with ThreadPoolExecutor(max_workers=args.concurrency) as ex:
        futures = {ex.submit(download_one, r, token, args.out, session): r for r in recs_to_download}
        with tqdm(total=len(futures), desc="Downloading", unit="file") as pbar:
            for fut in as_completed(futures):
                result = fut.result()
                results.append(result)
                # Update progress bar description with current status
                status = result.get("status", "unknown")
                pbar.set_postfix_str(f"last: {status}")
                pbar.update(1)

    # Write metadata report
    pathlib.Path(args.out).mkdir(parents=True, exist_ok=True)
    with open(pathlib.Path(args.out) / "download_report.json", "w", encoding="utf8") as fh:
        json.dump({
            "generated_at": datetime.datetime.now(datetime.UTC).isoformat(),
            "date_range": {"from": args.start, "to": args.end},
            "results": results
        }, fh, indent=2, ensure_ascii=False)

    # Print summary
    success = sum(1 for r in results if r["status"] == "ok")
    skipped = sum(1 for r in results if r["status"] == "skipped")
    errors = sum(1 for r in results if r["status"] in ("error", "exception", "no_url"))
    print(f"\nDone! Success: {success}, Skipped: {skipped}, Errors: {errors}")
    print(f"Report: {pathlib.Path(args.out) / 'download_report.json'}")


if __name__ == "__main__":
    main()
