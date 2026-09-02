"""Local-only staging bridge for the user's existing 9router Playwright worker.

This module deliberately does not implement an auth flow itself. It loads the user-selected
``auto_login.py`` worker, disables its debug screenshots, captures the OAuth connection objects in
memory, and writes one short-lived providerConnections JSON file for the Rust importer. Raw
credentials never appear in stdout/stderr or command-line arguments.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import sys
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path


MAX_ACCOUNTS = 100
MAX_WORKERS = 3


def load_worker(path: Path):
    spec = importlib.util.spec_from_file_location("ai_switcher_staging_auto_login", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("Could not load auto_login.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def read_accounts(path: Path):
    raw = path.read_text(encoding="utf-8")
    accounts = []
    seen = set()
    for line_number, line in enumerate(raw.splitlines(), start=1):
        value = line.strip()
        if not value or value.startswith("#"):
            continue
        parts = value.split("|")
        if len(parts) not in (2, 3):
            raise ValueError(f"Line {line_number} must use exactly email|password|2fa format")
        email = parts[0].strip()
        password = parts[1].strip()
        totp = parts[2].strip() if len(parts) == 3 else ""
        if not email or not password or "@" not in email:
            raise ValueError(f"Line {line_number} has an invalid account")
        key = email.casefold()
        if key in seen:
            raise ValueError(f"Line {line_number} repeats a duplicate email")
        seen.add(key)
        accounts.append((email, password, totp))
        if len(accounts) > MAX_ACCOUNTS:
            raise ValueError(f"Credential file contains too many accounts (maximum is {MAX_ACCOUNTS})")
    if not accounts:
        raise ValueError("Credential file contains no accounts")
    return accounts


def write_export(path: Path, connections):
    payload = json.dumps({"providerConnections": connections}, ensure_ascii=False).encode("utf-8")
    # The Rust parent pre-creates this path with an owner-only ACL. Truncate that protected file
    # instead of creating a new file with inherited temp-directory permissions on Windows.
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    fd = os.open(path, flags, 0o600)
    try:
        with os.fdopen(fd, "wb") as output:
            output.write(payload)
            output.flush()
    except Exception:
        try:
            os.close(fd)
        except OSError:
            pass
        raise


def run(args):
    if args.workers < 1 or args.workers > MAX_WORKERS:
        raise ValueError(f"Worker count must be between 1 and {MAX_WORKERS}")
    worker_path = Path(args.worker).resolve()
    accounts_path = Path(args.accounts).resolve()
    output_path = Path(args.output).resolve()
    if worker_path.name != "auto_login.py" or not worker_path.is_file():
        raise ValueError("The worker path must point to auto_login.py")
    if not accounts_path.is_file():
        raise ValueError("Credential file was not found")

    accounts = read_accounts(accounts_path)
    worker = load_worker(worker_path)
    connections = []
    connection_lock = threading.Lock()

    def capture_import(connection):
        # The worker expects the 9router import callback to return a verified response. The Rust
        # side performs its own strict OAuth validation before mutating app state.
        with connection_lock:
            connections.append(connection)
        return {"sqliteVerified": True, "total": 1}, None

    # The external worker's default debug hook writes screenshots containing login pages. Disable
    # it for the staging path; stdout/stderr are also redirected by the Rust parent process.
    worker.debug_page = lambda *_args, **_kwargs: None
    worker.import_to_9router = capture_import

    worker.start_callback_dispatcher()
    results = []
    try:
        active_workers = min(args.workers, len(accounts))
        with ThreadPoolExecutor(max_workers=active_workers, thread_name_prefix="ai-switcher-oauth") as executor:
            futures = {
                executor.submit(
                    worker.login_one_account,
                    index,
                    len(accounts),
                    account,
                    args.headed,
                    False,
                ): account[0]
                for index, account in enumerate(accounts, start=1)
            }
            for future in as_completed(futures):
                results.append(future.result())
    finally:
        worker.stop_callback_dispatcher()

    successful = [result for result in results if result.get("status") == "success"]
    if not successful or not connections:
        raise RuntimeError("No OAuth sessions were completed")
    connections.sort(key=lambda item: (item.get("email") or "").casefold())
    write_export(output_path, connections)


def main() -> int:
    parser = argparse.ArgumentParser(description="AI Account Switcher staging OAuth bridge")
    parser.add_argument("--worker", required=True)
    parser.add_argument("--accounts", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--workers", required=True, type=int)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--headed", action="store_true")
    mode.add_argument("--headless", action="store_true")
    args = parser.parse_args()
    try:
        run(args)
        return 0
    except Exception:
        # Keep errors generic: the Rust parent reports only the exit status and never replays
        # Python exceptions that might contain provider or filesystem details.
        return 2


if __name__ == "__main__":
    sys.exit(main())
