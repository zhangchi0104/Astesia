# Releasing

Versions are decided by [semantic-release](https://semantic-release.gitbook.io/)
from Conventional Commit messages on `main`. Every push to `main` runs
`.github/workflows/semantic-release.yml`:

| Commits since the last `v*` tag | Next version |
| --- | --- |
| `fix:` / `perf:` | patch |
| `feat:` | minor |
| `!` after the type, or a `BREAKING CHANGE:` footer | major |
| anything else (`ci:`, `docs:`, `refactor:`, ...) | no release |

When a release is due, semantic-release:

1. writes the new version into `Cargo.toml` and `Cargo.lock` with
   `scripts/set_version.py`, and prepends the notes to `CHANGELOG.md`;
2. commits them as `release: <version> [skip ci]` and tags `v<version>`;
3. creates the GitHub release with the generated notes;
4. calls the Native packages workflow (`release.yml`) for that tag, which
   builds, signs and notarizes the packages and uploads them to the release.

The release tag is pushed with `GITHUB_TOKEN`, and GitHub does not start other
workflows from events created by that token. The package build is therefore
invoked as a reusable workflow (`workflow_call`) rather than through its
`push: tags` trigger, so no personal access token is needed.

Pushing a `v*` tag by hand still runs the Native packages workflow and creates
a draft release, as before. The packaging scripts and the macOS `Info.plist`
read the version from `Cargo.toml`, so they always match the tag.

semantic-release needs at least one existing `v*` tag as its starting point;
the workflow fails instead of starting over at `1.0.0` if none is present.
