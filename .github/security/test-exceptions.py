"""Check exception policy boundaries without changing the repository's exception file."""
import datetime
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class ExceptionPolicy(unittest.TestCase):
    def check(self, entry: dict[str, str], expected: bool, *, rustsec: bool = False) -> None:
        with tempfile.TemporaryDirectory(prefix="trivy-exceptions-") as directory:
            root = Path(directory)
            policy = root / ".github/security"
            policy.mkdir(parents=True)
            shutil.copyfile(Path(__file__).with_name("check-exceptions.py"), policy / "check.py")
            (policy / "trivy-ignore.yaml").write_text(json.dumps({"vulnerabilities": [entry]}))
            ignore = '{ id = "RUSTSEC-2026-0387", reason = "Test mitigation" }' if rustsec else ""
            (root / "deny.toml").write_text(f"[advisories]\nignore = [{ignore}]\n")
            result = subprocess.run(
                [sys.executable, str(policy / "check.py")], capture_output=True, text=True, check=False
            )
            self.assertEqual(result.returncode == 0, expected, result.stderr)

    def entry(self, days: int) -> dict[str, str]:
        expiry = datetime.datetime.now(datetime.UTC) + datetime.timedelta(days=days)
        return {"id": "CVE-2026-1234", "statement": "Test mitigation", "expired_at": expiry.isoformat()}

    def test_reviewed_short_exception(self) -> None:
        self.check(self.entry(1), True)

    def test_missing_reason(self) -> None:
        entry = self.entry(1)
        entry["statement"] = " "
        self.check(entry, False)

    def test_missing_expiry(self) -> None:
        entry = self.entry(1)
        del entry["expired_at"]
        self.check(entry, False)

    def test_expired(self) -> None:
        self.check(self.entry(-1), False)

    def test_over_thirty_days(self) -> None:
        self.check(self.entry(31), False)

    def test_ambiguous_timezone(self) -> None:
        entry = self.entry(1)
        entry["expired_at"] = "2099-01-01T00:00:00"
        self.check(entry, False)

    def test_rustsec_review_and_expiry(self) -> None:
        for days, expected in [(1, True), (-1, False), (31, False)]:
            with self.subTest(days=days):
                entry = self.entry(days)
                entry["id"] = "RUSTSEC-2026-0387"
                self.check(entry, expected, rustsec=True)

    def test_cargo_ignore_without_review(self) -> None:
        self.check(self.entry(1), False, rustsec=True)

    def test_rustsec_review_without_cargo_ignore(self) -> None:
        entry = self.entry(1)
        entry["id"] = "RUSTSEC-2026-0387"
        self.check(entry, False)


if __name__ == "__main__":
    unittest.main()
