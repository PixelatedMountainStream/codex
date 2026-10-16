import contextlib
import io
import os
import json
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import sync_upstream as su  # noqa: E402

KEYS = {"ok", "step", "conflicts", "build_errors", "test_failures", "surface_violations", "next_action"}


HERMETIC = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main",
            "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"]


def _env():
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
    return env


def git(cwd, *a, check=True):
    return subprocess.run(["git", *HERMETIC, *a], cwd=cwd, check=check, capture_output=True, text=True, env=_env())


def run_cli(root, *args):
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        code = su.main(["--root", str(root), *args])
    return code, json.loads(buf.getvalue())


class SyncTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        t = Path(self.tmp.name)
        self.up, self.work = t / "up", t / "work"
        for d in (self.up, self.work):
            d.mkdir()
            git(d, "init", "-q")
        (self.up / "f").write_text("x")
        git(self.up, "add", "."); git(self.up, "commit", "-qm", "c")
        for tag in ["rust-v0.161.0", "rust-v0.162.1", "rust-v0.163.0-alpha.1", "rust-v0.163.0-rc.2",
                    "rust-v0.162.10", "rust-v0.164.0-beta.1", "other-v9.9.9"]:
            git(self.up, "tag", tag)
        (self.work / "local").mkdir()
        (self.work / "local" / "UPSTREAM_VERSION").write_text("rust-v0.162.1\n")
        (self.work / "local" / "fork-surface.toml").write_text('allow = ["codex-rs/core/src/hook.rs"]\n')
        git(self.work, "remote", "add", "upstream", str(self.up))

    def tearDown(self):
        self.tmp.cleanup()

    def test_status_newest_stable(self):
        code, out = run_cli(self.work, "status")
        self.assertEqual(code, 0)
        self.assertEqual(set(out) >= KEYS, True)
        self.assertEqual(out["latest"], "rust-v0.162.10")
        self.assertTrue(out["new_release"])
        self.assertEqual(out["next_action"], "merge rust-v0.162.10")

    def test_status_up_to_date(self):
        (self.work / "local" / "UPSTREAM_VERSION").write_text("rust-v0.162.10\n")
        code, out = run_cli(self.work, "status")
        self.assertFalse(out["new_release"])
        self.assertEqual(out["next_action"], "none")

    def test_classification(self):
        allow, gen = ["codex-rs/core/src/hook.rs"], ["codex-rs/Cargo.lock"]
        c = lambda p: su.classify(p, allow, gen)
        self.assertEqual(c("codex-rs/core/src/hook.rs"), "hook")
        self.assertEqual(c("codex-rs/local/cli/src/main.rs"), "local")
        self.assertEqual(c("local/FORK_SURFACE.md"), "local")
        self.assertEqual(c("codex-rs/Cargo.lock"), "generated")
        self.assertEqual(c("codex-rs/core/src/other.rs"), "other")

    def test_conflicts_clean_and_escalate(self):
        code, out = run_cli(self.work, "conflicts")
        self.assertEqual((code, out["ok"], out["conflicts"]), (0, True, []))
        # fabricate an unmerged path outside the allowlist
        git(self.work, "add", "."); git(self.work, "commit", "-qm", "base")
        git(self.work, "checkout", "-q", "-b", "m1"); (self.work / "z.rs").write_text("1")
        git(self.work, "add", "z.rs"); git(self.work, "commit", "-qm", "1")
        git(self.work, "checkout", "-q", "-b", "m2", "m1~1"); (self.work / "z.rs").write_text("2")
        git(self.work, "add", "z.rs"); git(self.work, "commit", "-qm", "2")
        git(self.work, "merge", "m1", check=False)  # expected to conflict
        self.assertTrue(git(self.work, "ls-files", "-u").stdout.strip(), "setup produced no conflict")
        code, out = run_cli(self.work, "conflicts")
        self.assertEqual(code, 2)
        self.assertEqual(out["conflicts"], [{"path": "z.rs", "kind": "other"}])
        self.assertTrue(out["next_action"].startswith("ESCALATE"))

    def test_missing_remote_escalates(self):
        git(self.work, "remote", "remove", "upstream")
        code, out = run_cli(self.work, "status")
        self.assertEqual(code, 2)
        self.assertEqual(set(out) >= KEYS, True)

    def _stub_surface(self, violations):
        d = self.work / "local" / "scripts"
        d.mkdir(parents=True)
        (d / "check_fork_surface.py").write_text(
            "import json,sys\nprint(json.dumps({'ok': False, 'violations': %r}))\nsys.exit(1)\n" % (violations,))

    def test_check_skipped_required_step_is_exit_1(self):
        code, out = run_cli(self.work, "check")   # no codex-rs, no fork-surface script
        self.assertEqual(code, 1)
        self.assertFalse(out["ok"])
        self.assertTrue(out["skipped"])

    def test_check_budget_and_non_allowlisted_escalate_exit_2(self):
        for v in ({"kind": "over_line_budget", "lines": 9, "max": 1},
                  {"kind": "not_allowlisted", "path": "codex-rs/x.rs"}):
            self._stub_surface([v])
            code, out = run_cli(self.work, "check")
            self.assertEqual(code, 2)
            self.assertTrue(out["next_action"].startswith("ESCALATE"))
            (self.work / "local" / "scripts" / "check_fork_surface.py").unlink()
            (self.work / "local" / "scripts").rmdir()

    def test_check_failed_workflow_disable_is_exit_1_and_listed(self):
        fail = subprocess.CompletedProcess([], 1, "", "no")
        with mock.patch.object(su, "new_workflows", return_value=["x.yml"]), \
                mock.patch.object(su.shutil, "which", return_value="/bin/gh"), \
                mock.patch.object(su, "run", return_value=fail):
            code, out = run_cli(self.work, "check", "--repo", "o/r")
        self.assertEqual(code, 1)
        self.assertEqual(out["workflows_to_disable"], ["x.yml"])

    def test_report_bad_check_json_is_structured_error(self):
        code, out = run_cli(self.work, "report", "--check-json", str(self.work / "nope.json"))
        self.assertEqual(code, 2)
        self.assertEqual(out["error"]["kind"], "bad_check_json")
        (self.work / "bad.json").write_text("{not json")
        code, out = run_cli(self.work, "report", "--check-json", str(self.work / "bad.json"))
        self.assertEqual(code, 2)

    def test_tag_validation(self):
        for bad in ("rust-v1.2.3-rc.1", "v1.2.3", "rust-v1.2.3\n"):
            self.assertEqual(run_cli(self.work, "status", "--tag", bad)[0], 2)
            self.assertEqual(run_cli(self.work, "merge", bad)[0], 2)
        self.assertEqual(run_cli(self.work, "status", "--tag", "rust-v0.163.0")[0], 0)


if __name__ == "__main__":
    unittest.main()
