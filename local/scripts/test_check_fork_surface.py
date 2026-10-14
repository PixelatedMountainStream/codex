import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_fork_surface.py")
TOML = 'allow = ["up/hook.rs"]\ngenerated = ["up/Cargo.lock"]\n[budget]\nmax_lines = 5\nmax_files = 2\n'


def git(root, *a):
    subprocess.run(["git", "-C", str(root), "-c", "user.name=t", "-c", "user.email=t@t",
                    "-c", "commit.gpgsign=false", *a], check=True, capture_output=True)


class CheckForkSurface(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        git(self.root, "init", "-q")
        self.write("local/UPSTREAM_VERSION", "v1\n")
        self.write("local/fork-surface.toml", TOML)
        self.write("up/hook.rs", "a\n")
        self.write("up/other.rs", "a\n")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-qm", "base")
        git(self.root, "tag", "v1")

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, text):
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    def run_check(self):
        git(self.root, "add", "-A")
        git(self.root, "commit", "-qm", "change")
        r = subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.root)],
                           capture_output=True, text=True)
        return r.returncode, json.loads(r.stdout)

    def test_ok(self):
        self.write("up/hook.rs", "a\nb\n")
        self.write("up/Cargo.lock", "x\n" * 100)  # generated: not budgeted
        self.write("codex-rs/local/x/lib.rs", "x\n" * 100)  # fork-owned
        code, out = self.run_check()
        self.assertEqual((code, out["ok"], out["total_lines"]), (0, True, 1))
        self.assertEqual([f["path"] for f in out["files"]], ["up/hook.rs"])

    def test_non_allowlisted_path(self):
        self.write("up/other.rs", "a\nb\n")
        code, out = self.run_check()
        self.assertEqual(code, 1)
        self.assertEqual(out["violations"], [{"kind": "not_allowlisted", "path": "up/other.rs"}])

    def test_over_budget(self):
        self.write("up/hook.rs", "a\n" + "b\n" * 9)
        code, out = self.run_check()
        self.assertEqual(code, 1)
        self.assertEqual([v["kind"] for v in out["violations"]], ["over_line_budget"])


if __name__ == "__main__":
    unittest.main()
