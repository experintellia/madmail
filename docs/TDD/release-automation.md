# Release automation

Public CI versions the repository and publishes **unsigned testing** builds. Stable binaries are still built and signed outside GitHub Actions. This repository does not contain the production publisher, signing keys, or registry credentials for that environment.

`madmail update` / `madmail upgrade` skip draft and prerelease GitHub Releases and still require the existing Ed25519 trailer. Unsigned unstable binaries are not accepted by that updater.

## Versioning

`scripts/release.py` runs on `main` after lint, tests, and cargo audit succeed. It looks at Conventional Commits since the highest merged `vX.Y.Z` or `vX.Y.Z-unstable` tag:

| Commits | Bump |
|---------|------|
| `fix:`, `perf:`, `revert:` | patch |
| `feat:` | minor |
| Breaking change (`!` or a `BREAKING CHANGE` footer) | major |
| `docs:`, `chore:`, `ci:`, `test:`, and other types | none |

The numeric manifests stay `X.Y.Z`. The `-unstable` suffix is only on the Git tag, GitHub prerelease, and container tags.

A release updates:

- `.version`
- `[workspace.package] version` in `Cargo.toml`
- workspace packages in `Cargo.lock`
- member manifests that do not inherit the workspace version
- `CHANGELOG.md`

The root `package.json` was only the semantic-release package and is not a version manifest. Landing and `external/madmail-admin-web` keep their own versions.

The version commit is `chore(release): X.Y.Z [skip ci]`, authored by `Madmail-CI <ci@madmail.chat>`. CI does not sign that commit or its annotated tag. Actions has no production signing key. A local `--sign` run can sign with the operator's own Git key; that is separate from binary signing.

The push is atomic and not forced. A rejected push leaves the remote branch and tags unchanged. The same local checkout can retry the push. A newer `main` tip is not overwritten; the later workflow cuts the release. Re-running a workflow does not add a second changelog heading or tag for the same version.

`python3 scripts/release.py` is a dry run. `--check` only compares the manifests. `--write --push` commits and pushes. Tests live in `scripts/test_release.py` and use temporary repositories.

## Unstable channel

When the version commit is created, the same workflow builds:

- the existing Linux `cargo build --release --locked --bin madmail` artifact
- an unsigned Windows amd64 `madmail.exe`

It does not build the private static production binary, Windows arm64 release binaries, or the Inno installer. Those stay on their current paths. The Linux CI binary is not the static binary from `scripts/build-release-static.sh`.

After that build and the earlier tests succeed, CI creates a GitHub **prerelease** `vX.Y.Z-unstable` whose notes start with:

> TESTING ONLY — NOT FOR PRODUCTION USE.
>
> These builds are unsigned, may contain regressions or incompatible changes, and do not have guaranteed upgrade or downgrade compatibility. They are not supported by the signed production updater.

Assets are `madmail-linux-amd64`, `madmail-windows-amd64.exe`, `SOURCE.txt` (the full source revision), and `SHA256SUMS`. Uploads do not use `--clobber`. A retry reuses matching bytes and fails if an existing asset differs.

The container tags for that same commit are:

| Tag | Meaning |
|-----|---------|
| `X.Y.Z-unstable` | Immutable. Created only when the registry returns HTTP 404. Reused when `org.opencontainers.image.revision` is this commit. Any other registry response fails the job. |
| `unstable` | Moving tag. Advanced only when this candidate is the highest `v*-unstable` tag on `main`. |

The unstable image is published only after that GitHub prerelease upload succeeds. A failed Linux build, Windows build, or prerelease upload publishes nothing. The image is built from the commit the `vX.Y.Z-unstable` tag resolves to. Non-release commits do not publish a prerelease or move either tag. `latest` is not updated here.

The moving `unstable` tag is chosen from `origin/main` after a fetch. A retry of an older commit sees a newer tag and leaves the alias where it is.

## Stable channel

Stable publication stays on the private publisher (`make publish` → local `scripts/publish.sh`, which is gitignored and is not replaced by this tree).

When a **published, non-prerelease** GitHub Release `vX.Y.Z` appears, `.github/workflows/stable-docker.yml` builds `ghcr.io/themadorg/madmail:X.Y.Z` from the commit that Git tag resolves to. Annotated tags are peeled. The workflow checks out that commit, then checks that `.version` and `[workspace.package]` equal `X.Y.Z`.

A full 40-character SHA in the release's `targetCommitish` must match the tag. A branch or tag name in that field is accepted. Historical stable tags do not need a `vX.Y.Z-unstable` tag. When that unstable tag exists, it must resolve to the same commit.

The version image uses the same 404-or-matching-revision rule. `latest` moves only when this version is the highest published stable semver. An older retry does not move `latest` backward. A registry or authentication failure is not treated as a missing image.

The workflow does not read private provenance files. Those can be checked later, when the private publisher is available, without changing the public versioning script.

## Windows installers

`windows.yml` builds `madmail-windows-amd64-setup.exe` only for a published stable release `vX.Y.Z`. Unstable prereleases do not run that job. Development smoke tests, the arm64 compile, and the Linux Windows-crate job do not run on a release event, and the installer job does not need them. An older release that no longer compiles on the current runner can still be packaged from its published binaries.

Packaging tools come from the repository default branch: `scripts/windows_release.py`, `scripts/verify_signed_binary.py`, `packaging/windows/Madmail.iss`, and `packaging/windows/build-setup.ps1`. The selected release is a separate checkout. The job fetches the `vX.Y.Z` tag there, including annotated tags, and reads `.version` and `[workspace.package]` from that commit. An older release does not need to contain the packaging scripts. The job does not run helper scripts from the release commit, and it does not build either binary from the default branch.

It downloads `madmail-windows-amd64.exe` from that GitHub Release and checks:

- the Ed25519 trailer (last 64 bytes) against the public key in `crates/chatmail/src/upgrade.rs`
- `madmail version` against the tag
- `.version` and `[workspace.package]` at the tagged commit
- a full commit SHA in the release's `targetCommitish` when that field is a SHA

A branch or tag name in `targetCommitish` is accepted. Those verified bytes are the server file Inno Setup packages. The installer version is that same `X.Y.Z`.

It also downloads `madmail-tray-windows-amd64.exe` from the same release and stages it at the path `Madmail.iss` already expects. The stable compile passes `/DRequireTray=1`, so a missing tray file fails Inno Setup instead of producing an installer without tray shortcuts or autostart. The tray is not checked with the server Ed25519 trailer. The tray program has no public version command that this job can run.

When the tray asset has a `sha256` digest, or `SHA256SUMS` lists it, those values must match the downloaded bytes. A checksum shows that the bytes match what the release published. It does not prove which commit built the tray. `SOURCE.txt`, when the release has one, must still be the tag commit; that is the release's claimed source, not a separate attestation inside the tray file. If neither a digest nor a `SHA256SUMS` entry is present, the job logs that and stages the release asset anyway. A missing tray asset fails the job.

The job uploads `madmail-windows-amd64-setup.exe` only. An identical installer is reused. A different published installer fails the job. The portable server executable and the tray executable are not uploaded or replaced. The smoke-test artifact is not reused.

The private publisher has to attach both `madmail-windows-amd64.exe` (Ed25519 trailer) and `madmail-tray-windows-amd64.exe` (built for that same release) before this job can succeed. If it publishes `SHA256SUMS`, that file must list the tray as well as the server. This repository does not define a new publishing protocol for that step. Local `build-setup.ps1` runs still treat the tray as optional unless `-RequireTray` is passed, and they do not upload anything to a release.

## What CI will not do

- No production Ed25519 keys, tokens, or private publish scripts in the repository or Actions.
- No new tracked `scripts/publish.sh`.
- No change to signed `update` / `upgrade`.
- No rewrite or deletion of existing tags.
- No stable image publish merely because `main` moved.
