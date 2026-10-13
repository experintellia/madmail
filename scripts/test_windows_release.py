"""Stable Windows installer policy. No live releases, keys, or uploads."""

import hashlib
import json
import stat
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

import yaml

import verify_signed_binary
import windows_release
from verify_signed_binary import VerifyError
from test_verify_signed_binary import ephemeral_private_key, public_hex, sign

REPO = Path(__file__).resolve().parents[1]
WORKFLOW = REPO / ".github/workflows/windows.yml"


def git(root, *args):
    result = subprocess.run(
        ["git", *args],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        raise AssertionError(result.stderr or result.stdout)
    return result.stdout.strip()


def init_repo(root):
    git(root, "init", "-b", "main")
    git(root, "config", "user.email", "test@example.com")
    git(root, "config", "user.name", "Test")


def commit_version(root, version, message="release"):
    (root / ".version").write_text(version + "\n", encoding="utf-8")
    (root / "Cargo.toml").write_text(
        textwrap.dedent(
            f"""\
            [workspace.package]
            version = "{version}"
            """
        ),
        encoding="utf-8",
    )
    git(root, "add", ".version", "Cargo.toml")
    git(root, "commit", "-m", message)
    return git(root, "rev-parse", "HEAD")


def release_meta(tag, assets, **extra):
    meta = {
        "tagName": tag,
        "isDraft": False,
        "isPrerelease": False,
        "targetCommitish": "main",
        "assets": [{"name": name} for name in assets],
    }
    meta.update(extra)
    return meta


class WindowsReleaseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        init_repo(self.root)
        self.commit = commit_version(self.root, "1.2.3")
        git(self.root, "tag", "v1.2.3", self.commit)
        self.private = ephemeral_private_key(self.root)
        self.public = public_hex(self.private, self.root)
        self.signed = sign(
            self.private,
            b"#!/bin/sh\nif [ \"$1\" = version ]; then echo madmail-v2 1.2.3; fi\nexit 0\n",
        )

    def patch_key(self):
        original = verify_signed_binary.PUBLIC_KEY_HEX
        verify_signed_binary.PUBLIC_KEY_HEX = self.public
        self.addCleanup(setattr, verify_signed_binary, "PUBLIC_KEY_HEX", original)

    def write_exe(self, folder):
        path = folder / windows_release.EXE_NAME
        path.write_bytes(self.signed)
        path.chmod(path.stat().st_mode | stat.S_IEXEC)
        return path

    def test_annotated_and_lightweight_tags_select_the_commit(self):
        self.patch_key()
        later = commit_version(self.root, "1.2.4", "later")
        git(self.root, "tag", "-a", "v1.2.3-annotated", "-m", "stable", self.commit)
        # The lightweight tag under test is v1.2.3; the annotated tag is separate.
        tag_object = git(self.root, "rev-parse", "v1.2.3-annotated")
        self.assertNotEqual(tag_object, self.commit)
        self.assertEqual(
            windows_release.peeled_commit(self.root, "v1.2.3-annotated"), self.commit
        )
        self.assertEqual(windows_release.peeled_commit(self.root, "v1.2.3"), self.commit)
        self.assertNotEqual(git(self.root, "rev-parse", "HEAD"), self.commit)
        self.assertEqual(later, git(self.root, "rev-parse", "HEAD"))
        exe = self.write_exe(self.root)
        dest = self.root / "build" / windows_release.EXE_NAME
        result = windows_release.prepare(
            self.root,
            "v1.2.3",
            release_meta("v1.2.3", [windows_release.EXE_NAME]),
            exe,
            self.root,
            dest,
        )
        self.assertEqual(result["sha"], self.commit)
        self.assertEqual(result["version"], "1.2.3")
        self.assertEqual(dest.read_bytes(), self.signed)

    def test_bad_signature_unsigned_file_and_version_mismatch_do_not_stage(self):
        self.patch_key()
        exe = self.write_exe(self.root)
        dest = self.root / "out.exe"
        tampered = self.root / windows_release.EXE_NAME
        data = bytearray(exe.read_bytes())
        data[0] ^= 0x01
        tampered.write_bytes(data)
        meta = release_meta("v1.2.3", [windows_release.EXE_NAME])
        with self.assertRaisesRegex(VerifyError, "Ed25519 signature rejected"):
            windows_release.prepare(self.root, "v1.2.3", meta, tampered, self.root, dest)
        self.assertFalse(dest.exists())
        unsigned = self.root / "unsigned" / windows_release.EXE_NAME
        unsigned.parent.mkdir()
        unsigned.write_bytes(b"not-signed" + b"\0" * 64)
        with self.assertRaisesRegex(VerifyError, "Ed25519 signature rejected"):
            windows_release.prepare(self.root, "v1.2.3", meta, unsigned, self.root, dest)
        self.assertFalse(dest.exists())
        exe.write_bytes(self.signed)
        with self.assertRaisesRegex(VerifyError, "does not match"):
            windows_release.prepare(
                self.root,
                "v1.2.3",
                meta,
                exe,
                self.root,
                dest,
                version_text="madmail-v2 9.9.9\n",
            )
        self.assertFalse(dest.exists())

    def test_missing_prerelease_and_inconsistent_metadata_fail(self):
        self.patch_key()
        exe = self.write_exe(self.root)
        dest = self.root / "out.exe"
        with self.assertRaisesRegex(VerifyError, "not on the release"):
            windows_release.prepare(
                self.root,
                "v1.2.3",
                release_meta("v1.2.3", []),
                exe,
                self.root,
                dest,
            )
        with self.assertRaisesRegex(VerifyError, "draft or prerelease"):
            windows_release.prepare(
                self.root,
                "v1.2.3",
                release_meta("v1.2.3", [windows_release.EXE_NAME], isPrerelease=True),
                exe,
                self.root,
                dest,
            )
        with self.assertRaisesRegex(VerifyError, "vX.Y.Z"):
            windows_release.prepare(
                self.root,
                "v1.2.3-unstable",
                release_meta("v1.2.3-unstable", [windows_release.EXE_NAME]),
                exe,
                self.root,
                dest,
            )
        with self.assertRaisesRegex(VerifyError, "commit SHA"):
            windows_release.prepare(
                self.root,
                "v1.2.3",
                release_meta(
                    "v1.2.3",
                    [windows_release.EXE_NAME],
                    targetCommitish="b" * 40,
                ),
                exe,
                self.root,
                dest,
            )
        (self.root / ".version").write_text("9.9.9\n", encoding="utf-8")
        git(self.root, "add", ".version")
        git(self.root, "commit", "-m", "wrong tree is not the tag")
        # The tagged tree still matches. A wrong SHA cross-check was above.
        windows_release.prepare(
            self.root,
            "v1.2.3",
            release_meta("v1.2.3", [windows_release.EXE_NAME], targetCommitish=self.commit),
            exe,
            self.root,
            dest,
        )
        self.assertEqual(dest.read_bytes(), self.signed)

    def test_manifest_checksum_and_source_mismatches_fail(self):
        self.patch_key()
        other = commit_version(self.root, "9.9.9", "other")
        git(self.root, "tag", "-d", "v1.2.3")
        git(self.root, "tag", "v1.2.3", other)
        exe = self.write_exe(self.root)
        with self.assertRaisesRegex(VerifyError, r"\.version"):
            windows_release.prepare(
                self.root,
                "v1.2.3",
                release_meta("v1.2.3", [windows_release.EXE_NAME]),
                exe,
                self.root,
                self.root / "nope.exe",
            )
        git(self.root, "tag", "-d", "v1.2.3")
        git(self.root, "tag", "v1.2.3", self.commit)
        digest = hashlib.sha256(self.signed).hexdigest()
        sums = self.root / "SHA256SUMS"
        sums.write_text(f"{'ab' * 32}  {windows_release.EXE_NAME}\n", encoding="utf-8")
        meta = release_meta(
            "v1.2.3",
            [windows_release.EXE_NAME, "SHA256SUMS"],
        )
        with self.assertRaisesRegex(VerifyError, "SHA256SUMS"):
            windows_release.prepare(self.root, "v1.2.3", meta, exe, self.root, self.root / "nope.exe")
        sums.write_text(f"{digest}  {windows_release.EXE_NAME}\n", encoding="utf-8")
        source = self.root / "SOURCE.txt"
        source.write_text("c" * 40 + "\n", encoding="utf-8")
        meta["assets"].append({"name": "SOURCE.txt"})
        with self.assertRaisesRegex(VerifyError, "SOURCE.txt"):
            windows_release.prepare(self.root, "v1.2.3", meta, exe, self.root, self.root / "nope.exe")
        source.write_text(self.commit + "\n", encoding="utf-8")
        meta["assets"][0]["digest"] = "sha256:" + digest
        dest = self.root / "staged.exe"
        # dest name is not required to be the asset name; the downloaded file is.
        result = windows_release.prepare(self.root, "v1.2.3", meta, exe, self.root, dest)
        self.assertEqual(result["sha"], self.commit)
        self.assertEqual(dest.read_bytes(), self.signed)

    def test_installer_conflict_reuses_identical_bytes_and_never_uploads_the_exe(self):
        fresh = b"installer-bytes"
        self.assertEqual(windows_release.installer_plan(None, fresh), "upload")
        self.assertEqual(windows_release.installer_plan(fresh, fresh), "reuse")
        with self.assertRaisesRegex(VerifyError, "conflicts"):
            windows_release.installer_plan(b"other", fresh)
        meta = release_meta("v1.2.3", [windows_release.EXE_NAME, windows_release.SETUP_NAME])
        self.assertEqual(
            windows_release.decide_upload("v1.2.3", meta, fresh, fresh), "reuse"
        )
        with self.assertRaisesRegex(VerifyError, "draft or prerelease"):
            windows_release.decide_upload(
                "v1.2.3",
                release_meta("v1.2.3", [windows_release.SETUP_NAME], isPrerelease=True),
                fresh,
                None,
            )
        setup = self.root / windows_release.SETUP_NAME
        setup.write_bytes(fresh)
        command = windows_release.upload_command("v1.2.3", setup)
        self.assertEqual(command[:4], ["gh", "release", "upload", "v1.2.3"])
        self.assertNotIn("--clobber", command)
        self.assertNotIn(windows_release.EXE_NAME, command)
        self.assertTrue(command[-1].endswith(windows_release.SETUP_NAME))
        with self.assertRaisesRegex(VerifyError, "except the installer"):
            windows_release.upload_command("v1.2.3", self.root / windows_release.EXE_NAME)

    def test_stage_downloads_the_signed_executable_and_does_not_upload(self):
        self.patch_key()
        calls = []

        def runner(args, cwd, timeout):
            calls.append(list(args))
            if args[:2] == ["git", "fetch"]:
                return ""
            if args[:3] == ["gh", "release", "view"]:
                return json.dumps(release_meta("v1.2.3", [windows_release.EXE_NAME]))
            if args[:3] == ["gh", "release", "download"]:
                folder = Path(args[args.index("--dir") + 1])
                self.write_exe(folder)
                return ""
            raise AssertionError(args)

        dest = self.root / "build" / windows_release.EXE_NAME
        result = windows_release.stage(self.root, "v1.2.3", dest, runner=runner)
        self.assertEqual(result["sha"], self.commit)
        self.assertEqual(dest.read_bytes(), self.signed)
        flat = [" ".join(call) for call in calls]
        self.assertFalse(any("upload" in call or "--clobber" in call for call in flat))
        self.assertTrue(any(windows_release.EXE_NAME in call for call in flat))

    def test_plan_upload_blocks_conflicts_and_prereleases(self):
        setup = self.root / windows_release.SETUP_NAME
        setup.write_bytes(b"new-installer")
        calls = []

        def view(assets, **extra):
            return json.dumps(release_meta("v1.2.3", assets, **extra))

        def runner(args, cwd, timeout):
            calls.append(list(args))
            if args[:3] == ["gh", "release", "view"]:
                return view([windows_release.EXE_NAME, windows_release.SETUP_NAME])
            if args[:3] == ["gh", "release", "download"]:
                folder = Path(args[args.index("--dir") + 1])
                (folder / windows_release.SETUP_NAME).write_bytes(b"published-installer")
                return ""
            raise AssertionError(args)

        with self.assertRaisesRegex(VerifyError, "conflicts"):
            windows_release.plan_upload(self.root, "v1.2.3", setup, runner=runner)
        self.assertFalse(any(call[2] == "upload" for call in calls if call[:2] == ["gh", "release"]))

        calls.clear()

        def reuse_runner(args, cwd, timeout):
            calls.append(list(args))
            if args[:3] == ["gh", "release", "view"]:
                return view([windows_release.SETUP_NAME])
            if args[:3] == ["gh", "release", "download"]:
                folder = Path(args[args.index("--dir") + 1])
                (folder / windows_release.SETUP_NAME).write_bytes(b"new-installer")
                return ""
            raise AssertionError(args)

        self.assertEqual(
            windows_release.plan_upload(self.root, "v1.2.3", setup, runner=reuse_runner),
            "reuse",
        )
        self.assertFalse(any("upload" in " ".join(call) for call in calls))

        calls.clear()

        def upload_runner(args, cwd, timeout):
            calls.append(list(args))
            if args[:3] == ["gh", "release", "view"]:
                return view([windows_release.EXE_NAME])
            if args[:3] == ["gh", "release", "upload"]:
                return ""
            raise AssertionError(args)

        self.assertEqual(
            windows_release.plan_upload(self.root, "v1.2.3", setup, runner=upload_runner),
            "upload",
        )
        upload = [call for call in calls if "upload" in call]
        self.assertEqual(len(upload), 1)
        self.assertNotIn("--clobber", upload[0])
        self.assertNotIn(windows_release.EXE_NAME, upload[0])

        def prerelease_runner(args, cwd, timeout):
            if args[:3] == ["gh", "release", "view"]:
                return view([windows_release.SETUP_NAME], isPrerelease=True)
            raise AssertionError(args)

        with self.assertRaisesRegex(VerifyError, "draft or prerelease"):
            windows_release.plan_upload(self.root, "v1.2.3", setup, runner=prerelease_runner)

    def test_workflow_packages_only_a_verified_stable_installer(self):
        source = WORKFLOW.read_text(encoding="utf-8")
        document = yaml.safe_load(source)
        setup = document["jobs"]["windows-amd64-setup"]
        smoke = document["jobs"]["windows-amd64-smoke"]
        rendered = yaml.dump(setup)
        self.assertEqual(
            setup["if"],
            "github.event_name == 'release' && github.event.release.prerelease == false",
        )
        self.assertIn("windows_release.py stage", source)
        self.assertIn("windows_release.py plan-upload", source)
        self.assertIn('-Version "${{ steps.signed.outputs.version }}"', source)
        self.assertNotIn("--clobber", rendered)
        self.assertNotIn("gh release upload", rendered)
        self.assertNotIn("madmail-windows-amd64-ci", rendered)
        self.assertNotIn("download-artifact", rendered)
        self.assertNotIn("--clobber", WORKFLOW.read_text(encoding="utf-8"))
        self.assertNotIn("--clobber", (REPO / "scripts/windows_release.py").read_text(encoding="utf-8"))
        self.assertIn("github.event_name != 'release'", smoke["if"])
        self.assertIn("cargo build", yaml.dump(smoke))
        iss = (REPO / "packaging/windows/Madmail.iss").read_text(encoding="utf-8")
        script = (REPO / "packaging/windows/build-setup.ps1").read_text(encoding="utf-8")
        self.assertIn("#ifndef MyAppVersion", iss)
        self.assertNotIn('MyAppVersion "2.17.3"', iss)
        self.assertIn('"/DMyAppVersion=$AppVersion"', script)
        self.assertIn("Resolve-AppVersion", script)


if __name__ == "__main__":
    unittest.main()
