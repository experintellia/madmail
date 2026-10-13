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

`windows.yml` builds `madmail-windows-amd64-setup.exe` only for a published stable release `vX.Y.Z`. Unstable prereleases do not run that job.

The job resolves the Git tag, including annotated tags, and downloads the `madmail-windows-amd64.exe` asset already on that release. It checks:

- the Ed25519 trailer (last 64 bytes) against the public key in `crates/chatmail/src/upgrade.rs`
- `madmail version` against the tag
- `.version` and `[workspace.package]` at the tagged commit
- a full commit SHA in the release's `targetCommitish` when that field is a SHA

A branch or tag name in `targetCommitish` is accepted. The verified bytes are the file Inno Setup packages. The installer version is that same `X.Y.Z`.

Optional checks, used only when the release already has them: a GitHub asset `digest` of `sha256:…`, a `SHA256SUMS` line for `madmail-windows-amd64.exe` (hash of the published bytes, trailer included), and `SOURCE.txt` containing the tag commit. A missing optional file is ignored. A present file that does not match fails the job. No private provenance file is required.

The job uploads `madmail-windows-amd64-setup.exe` only. An identical installer is reused. A different published installer fails the job. The portable executable is not uploaded or replaced.

The private publisher still has to attach the signed `madmail-windows-amd64.exe` to the stable release before this job can succeed. This repository does not define a new publishing protocol for that step. A signed tray asset is not required; the generated installer omits the tray when `build/madmail-tray-windows-amd64.exe` is absent. Smoke tests and local `build-setup.ps1` runs continue to build unsigned development binaries and do not upload them to a release.

## What CI will not do

- No production Ed25519 keys, tokens, or private publish scripts in the repository or Actions.
- No new tracked `scripts/publish.sh`.
- No change to signed `update` / `upgrade`.
- No rewrite or deletion of existing tags.
- No stable image publish merely because `main` moved.
