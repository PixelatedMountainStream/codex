"""Tiny tests for install.py / mock_ollama. Temp dirs only; never touches the real ~/.codex."""

import hashlib
import json
import sys
import tempfile
import unittest
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import _lib  # noqa: E402
import install  # noqa: E402
import mock_ollama  # noqa: E402

PS = ["  101 /usr/bin/ssh -N -o ExitOnForwardFailure=yes -L 11435:127.0.0.1:11434 gpu1",
      "  102 /usr/bin/ssh -N -L 8080:127.0.0.1:80 web",
      "  103 vim ssh -N -L 1:127.0.0.1:11434"]


def releases():
    return [
        {"tag_name": "rust-v0.200.0", "published_at": "2026-10-10T00:00:00Z"},
        {"tag_name": "local-v0.1.0", "published_at": "2026-10-01T00:00:00Z"},
        {"tag_name": "local-v0.2.0", "published_at": "2026-10-05T00:00:00Z",
         "assets": [{"name": "codex-local-x86_64-pc-windows-msvc.exe"}]},
        {"tag_name": "local-v0.3.0", "draft": True, "published_at": "2026-10-09T00:00:00Z"},
    ]


class WebSearchRunner(unittest.TestCase):
    def test_missing_runner_detected(self):
        from unittest import mock
        with mock.patch.object(_lib.shutil, "which", return_value=None):
            self.assertTrue(_lib.web_search_runner_missing())
        with mock.patch.object(_lib.shutil, "which", return_value="/x/uvx"):
            self.assertFalse(_lib.web_search_runner_missing())


class Selection(unittest.TestCase):
    def test_newest_local_ignores_rust_and_drafts(self):
        self.assertEqual(install.select_release(releases())["tag_name"], "local-v0.2.0")

    def test_pinned_tag_and_none(self):
        self.assertEqual(install.select_release(releases(), "local-v0.1.0")["tag_name"], "local-v0.1.0")
        self.assertIsNone(install.select_release([{"tag_name": "rust-v1"}]))

    def test_asset_names(self):
        self.assertEqual(_lib.asset_name("aarch64-apple-darwin"), "codex-local-aarch64-apple-darwin")
        self.assertEqual(_lib.asset_name("x86_64-pc-windows-msvc"), "codex-local-x86_64-pc-windows-msvc.exe")
        self.assertIsNotNone(install.find_asset(releases()[2], "codex-local-x86_64-pc-windows-msvc.exe"))

    def test_repo_default(self):
        self.assertEqual(_lib.DEFAULT_REPO, "PixelatedMountainStream/codex")


class Sha(unittest.TestCase):
    def test_verify(self):
        data = b"binary"
        h = hashlib.sha256(data).hexdigest()
        sums = f"{h}  codex-local-x\n{'0' * 64} *other\n"
        self.assertEqual(install.verify_asset(data, "codex-local-x", sums), h)
        with self.assertRaises(ValueError):
            install.verify_asset(b"tampered", "codex-local-x", sums)
        with self.assertRaises(ValueError):
            install.verify_asset(data, "missing", sums)

    def test_version_parse(self):
        self.assertEqual(_lib.parse_version("codex-cli 0.162.1+local.abc1"), ("0.162.1", "+local.abc1"))
        self.assertEqual(_lib.parse_version("codex-cli 0.162.1"), ("0.162.1", ""))


class Legacy(unittest.TestCase):
    def setUp(self):
        self._td = tempfile.TemporaryDirectory()
        self.stock = Path(self._td.name) / "stock"
        self.local = Path(self._td.name) / "local"
        (self.stock / "agents").mkdir(parents=True)
        (self.stock / "agents" / "legacy.toml").write_text('wire_api = "chat"\n')
        (self.stock / "agents" / "keep.toml").write_text('name = "keep"\n')
        (self.stock / "ollama.config.toml").write_text("model_apply_patch_tool_type = \"function\"\n")
        self.cfg = 'model = "x"\n[model_providers.a]\nwire_api = "chat"\nname = "n"\n'
        (self.stock / "config.toml").write_text(self.cfg)

    def tearDown(self):
        self._td.cleanup()

    def test_matching_by_content(self):
        f = install.scan_legacy(self.stock, PS)
        paths = {x["path"].name for x in f if "path" in x}
        self.assertEqual(paths, {"legacy.toml", "ollama.config.toml", "config.toml"})
        self.assertEqual([x["pid"] for x in f if x["kind"] == "process"], [101])

    def test_dry_run_reports_and_changes_nothing(self):
        before = {p: p.read_text() for p in self.stock.rglob("*") if p.is_file()}
        f = install.legacy_cleanup(self.stock, self.local, True, True, PS)
        self.assertEqual(len(f), 4)
        self.assertEqual(before, {p: p.read_text() for p in self.stock.rglob("*") if p.is_file()})
        self.assertFalse(self.local.exists())

    def test_non_interactive_reports_only(self):
        install.legacy_cleanup(self.stock, self.local, False, False, PS)
        self.assertTrue((self.stock / "agents" / "legacy.toml").exists())

    def test_backup_then_remove(self):
        killed = []
        f = install.scan_legacy(self.stock, PS)
        bk = self.local / "legacy-backup" / "t"
        install.backup_and_remove(f, self.stock, bk, kill=killed.append)
        self.assertFalse((self.stock / "agents" / "legacy.toml").exists())
        self.assertTrue((self.stock / "agents" / "keep.toml").exists())
        self.assertEqual((bk / "agents" / "legacy.toml").read_text(), 'wire_api = "chat"\n')
        self.assertEqual((bk / "config.toml").read_text(), self.cfg)
        self.assertEqual((self.stock / "config.toml").read_text(),
                         'model = "x"\n[model_providers.a]\nname = "n"\n')
        self.assertEqual(killed, [101])

    def test_multiline_string_mentioning_key_is_not_matched(self):
        text = ('developer_instructions = """\nnever write\nwire_api = "chat"\n'
                'model_apply_patch_tool_type = "x"\n"""\n')
        self.assertFalse(install.is_legacy_toml(text))
        (self.stock / "agents" / "legacy.toml").write_text(text)
        (self.stock / "ollama.config.toml").unlink()
        (self.stock / "config.toml").write_text(text)
        f = install.scan_legacy(self.stock, [])
        self.assertEqual(f, [])

    def test_config_edit_removes_only_real_keys_and_refuses_unsafe(self):
        text = ('developer_instructions = """\nwire_api = "chat"\n"""\n'
                '[p]\nwire_api = "chat"  # c\nmodel_apply_patch_tool_type = "function"\nname = "n"\n')
        out = install.strip_config_keys(text)
        self.assertEqual(out, 'developer_instructions = """\nwire_api = "chat"\n"""\n[p]\nname = "n"\n')
        # a quoted key is a real key but not locatable by line -> verification fails, skipped
        self.assertIsNone(install.strip_config_keys('"wire_api" = "chat"\n'))
        # inline-table key cannot be removed by line -> skipped, file untouched
        self.assertIsNone(install.strip_config_keys('p = {wire_api = "chat"}\n'))


class Mock(unittest.TestCase):
    def test_tags_and_responses_sse(self):
        srv, url = mock_ollama.start()
        try:
            tags = json.load(urllib.request.urlopen(url + "/api/tags"))
            self.assertEqual(tags["models"][0]["name"], "qwen3.8:27b")
            req = urllib.request.Request(url + "/v1/responses", b'{"stream": true}',
                                         {"Content-Type": "application/json"})
            body = urllib.request.urlopen(req).read().decode()
            self.assertIn("response.output_text.delta", body)
            self.assertIn("response.completed", body)
        finally:
            srv.shutdown()


class InstallRecord(unittest.TestCase):
    def test_installed_sha256_passes_verify(self):
        import verify_install
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            exe = td / "codex-local"
            exe.write_bytes(b"resigned")
            (td / "install.json").write_text(json.dumps(
                {"sha256": "0" * 64, "installed_sha256": _lib.sha256_file(exe)}))
            rep = verify_install.Report()
            verify_install.check_sha(rep, str(exe), td, None)
            self.assertEqual(rep.failed, 0)

    def test_missing_release_dir_asset_exits_1(self):
        import contextlib
        import io
        with tempfile.TemporaryDirectory() as td:
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = install.main(["--release-dir", td, "--bin-dir", td, "--stock-home", td,
                                   "--local-home", td, "--no-cleanup", "--no-verify"])
            self.assertEqual(rc, 1)
            self.assertIn("SHA256SUMS", out.getvalue())


if __name__ == "__main__":
    unittest.main()
