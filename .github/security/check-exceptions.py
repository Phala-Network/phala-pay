"""Require a reason and a future expiry for vulnerability exceptions."""
import datetime
import json
import tomllib
from pathlib import Path

path = Path(__file__).with_name("trivy-ignore.yaml")
# JSON is a YAML subset; keeping this file in JSON avoids another parser dependency.
data = json.loads(path.read_text())
if set(data) != {"vulnerabilities"} or not isinstance(data["vulnerabilities"], list):
    raise SystemExit("Only vulnerability exceptions are supported")
for entry in data["vulnerabilities"]:
    if not isinstance(entry.get("id"), str) or not entry["id"].strip():
        raise SystemExit("Exception needs an advisory id")
    if not isinstance(entry.get("statement"), str) or not entry["statement"].strip():
        raise SystemExit(f"{entry['id']}: exception needs a reason (statement)")
    expiry = datetime.datetime.fromisoformat(entry["expired_at"])
    now = datetime.datetime.now(datetime.UTC)
    if expiry.tzinfo is None or not now < expiry <= now + datetime.timedelta(days=30):
        raise SystemExit(f"{entry['id']}: exception expiry must be in the next 30 days")

# cargo-deny has no expiry field. Keep its review metadata in the same policy file
# and check it during `make lint` as well as every Trivy scan.
deny = tomllib.loads(path.parents[2].joinpath("deny.toml").read_text())
ignored = {entry["id"] for entry in deny["advisories"]["ignore"]}
reviewed = {entry["id"] for entry in data["vulnerabilities"] if entry["id"].startswith("RUSTSEC-")}
# The pre-existing unmaintained, compile-time paste exception is documented in CONTRIBUTING.md.
legacy = {"RUSTSEC-2024-0436"}
if ignored - legacy != reviewed:
    raise SystemExit("cargo-deny ignores must match the reviewed RustSec exceptions")
