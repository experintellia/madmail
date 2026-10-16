"""Publication order for the unstable channel. No registry or GitHub calls."""

import contextlib
import io
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml

import release

REPO = Path(__file__).resolve().parents[1]
CI = REPO / ".github/workflows/ci.yml"


def git(root, *args):
    result = subprocess.run(
        ["git", *args], cwd=root, text=True, capture_output=True, check=False
    )
    if result.returncode:
        raise AssertionError(result.stderr or result.stdout)
    return result.stdout.strip()


class UnstablePublicationTests(unittest.TestCase):
    def test_docker_waits_for_the_prerelease_and_does_not_move_latest(self):
        document = yaml.safe_load(CI.read_text(encoding="utf-8"))
        docker = document["jobs"]["docker-build-push"]
        prerelease = document["jobs"]["unstable-prerelease"]
        self.assertEqual(
            docker["needs"], ["release", "build", "test", "unstable-prerelease"]
        )
        self.assertIn("needs.unstable-prerelease.result == 'success'", docker["if"])
        self.assertIn("needs.release.outputs.tag != ''", docker["if"])
        self.assertIn("needs.build.result == 'success'", docker["if"])
        self.assertIn("needs.test.result == 'success'", docker["if"])
        self.assertEqual(
            prerelease["needs"], ["release", "build", "test", "unstable-windows"]
        )
        self.assertIn("needs.unstable-windows.result == 'success'", prerelease["if"])
        confirm = next(
            step for step in docker["steps"] if step.get("name") == "Confirm the unstable tag commit"
        )
        self.assertIn("${TAG}^{commit}", confirm["run"])
        build = next(
            step for step in docker["steps"] if step.get("name") == "Build immutable unstable image"
        )
        self.assertNotIn("latest", build["with"]["tags"])
        self.assertIn("needs.release.outputs.sha", build["with"]["labels"])
        advance = next(
            step
            for step in docker["steps"]
            if step.get("name") == "Advance unstable alias only for the newest unstable version"
        )
        self.assertIn("--revision origin/main", advance["run"])
        self.assertIn('"$IMAGE:unstable"', advance["run"])
        self.assertNotIn("latest", advance["run"])

    def test_older_checkout_cannot_become_the_unstable_tip(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            git(root, "init", "-b", "main")
            git(root, "config", "user.email", "test@example.com")
            git(root, "config", "user.name", "Test")
            (root / "note").write_text("one\n", encoding="utf-8")
            git(root, "add", "note")
            git(root, "commit", "-m", "first")
            old = git(root, "rev-parse", "HEAD")
            git(root, "tag", "-a", "v1.0.0-unstable", "-m", "old")
            (root / "note").write_text("two\n", encoding="utf-8")
            git(root, "add", "note")
            git(root, "commit", "-m", "second")
            git(root, "tag", "-a", "v2.0.0-unstable", "-m", "new")
            git(root, "checkout", "--detach", old)
            self.assertEqual(
                release.channel_tip(root, "unstable", revision="HEAD"),
                "v1.0.0-unstable",
            )
            self.assertEqual(
                release.channel_tip(root, "unstable", revision="main"),
                "v2.0.0-unstable",
            )
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = release.main(
                    [
                        "--repo",
                        str(root),
                        "--channel-tip",
                        "unstable",
                        "--revision",
                        "main",
                    ]
                )
            self.assertEqual(code, 0)
            self.assertEqual(stdout.getvalue().strip(), "v2.0.0-unstable")
            errors = io.StringIO()
            with contextlib.redirect_stderr(errors):
                rejected = release.main(
                    ["--repo", str(root), "--channel-tip", "unstable", "--revision", "origin/../main"]
                )
            self.assertEqual(rejected, 1)
            self.assertIn("Invalid revision", errors.getvalue())
            with contextlib.redirect_stderr(io.StringIO()) as errors:
                plain = release.main(["--repo", str(root), "--revision", "main"])
            self.assertEqual(plain, 1)
            self.assertIn("--revision requires --channel-tip", errors.getvalue())


if __name__ == "__main__":
    unittest.main()
