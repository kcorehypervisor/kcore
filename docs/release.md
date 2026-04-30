# Local Release Process (Make + Nix + GitHub Releases)

Releases run from an operator machine, not GitHub Actions. One command creates
and pushes the Git tag, builds the ISO and `kctl` with Nix, packages
`dist/`, then creates or updates the GitHub Release assets.

## Version sources (policy)

| Source | Role |
|--------|------|
| [`VERSION`](../VERSION) (single line, e.g. `0.2.0`) | **Product / packaging version**: Nix `kcoreVersion`, ISO filename `kcoreos-$(VERSION)-x86_64-linux.iso`, Git tag `v$(VERSION)`, release assets. **Bump this for every release.** |
| `crates/*/Cargo.toml` `version = "…"` | Rust crate semver. **Not automatically tied to `VERSION`.** This repo usually bumps crate versions in the same PR as `VERSION` so `kctl --version` matches the product version. |

## Preconditions

- The version bump PR is merged to `main`, including [`VERSION`](../VERSION)
  (and crate versions, when applicable).
- Run on Linux **x86_64** with Nix/flakes working.
- `gh` is available and authenticated from the Nix dev shell. Use
  `nix develop --command gh auth status` to verify, or set `GH_TOKEN` for
  non-interactive use. If a local `.env` exists, `scripts/release.sh publish`
  sources it before invoking `gh`, so `GH_TOKEN=...` in `.env` is enough for
  `make release`.
- The working tree is clean before `make release`; the script tags the exact
  checked-out commit.

## Steps

1. **Bump version**  
   Edit [`VERSION`](../VERSION) to `X.Y.Z` (and optionally align `crates/kctl/Cargo.toml` and other crates if you follow the policy above). Open a PR, get CI green, merge to `main`.

2. **Run the local release**

   ```bash
   git fetch origin main && git checkout main && git pull
   make release
   ```

   `make release` runs [`scripts/release.sh`](../scripts/release.sh) `release`:

   - validates `VERSION`,
   - creates annotated tag `v$(cat VERSION)` if missing,
   - verifies an existing local or remote tag points at the current commit,
   - pushes the tag to `origin`,
   - builds the ISO and `kctl` with Nix,
   - packages `dist/`,
   - creates the GitHub Release through `gh api`,
   - uploads release assets one by one with `gh release upload --clobber`.

## Individual targets

```bash
make release-tag
make release-build
make release-dist
make release-publish
```

The release packaging step produces:

- `dist/kctl-$(VERSION)-linux-x86_64.tar.gz` (binary at archive root: `kctl`)
- `dist/kcoreos-$(VERSION)-x86_64-linux.iso` (release asset name; copied from the single ISO produced under `result-iso/iso/`)
- `dist/SHA256SUMS` for both files

By default publishing uses GitHub-generated release notes unless `RELEASE_NOTES.md`
exists. To force custom notes:

```bash
RELEASE_NOTES=path/to/notes.md GH_TOKEN=... make release-publish
```

## Artifact notes

- **kctl** in the tarball is the **Nix-built** `kctl` from `.#kctl` (same lineage as the ISO), not a raw `cargo build`.
- **Platform**: **linux x86_64** (glibc via Nix). No musl/static build in this flow.
- **Large files**: ISOs are ~1–2 GiB; GitHub per-file limit is 2 GiB. Stay under that or split hosting for huge artifacts.

## Troubleshooting

- **Tag already exists on a different commit**: the script stops rather than moving a release tag. Check out the tagged commit or intentionally delete/recreate the tag yourself.
- **`gh release create` fails on `--verify-tag`**: run `make release-tag` first, or check that the remote tag exists: `git ls-remote origin refs/tags/vX.Y.Z`.
- **Wrong ISO name**: Nix may place the built ISO under a NixOS-derived name in `result-iso/iso/`; the dist step discovers the single `*.iso` there and copies it to the release asset name `kcoreos-$(VERSION)-x86_64-linux.iso`.
- **Publish authentication**: run `nix develop --command gh auth status`, or set `GH_TOKEN` in the environment / local `.env` for non-interactive `gh`. A classic token with `repo` scope works; fine-grained PATs need release-capable repository permissions (`Contents: Read and write`, and in some cases `Workflows: Read and write`).
- **Large ISO uploads**: `scripts/release.sh publish` intentionally creates the release record first and uploads assets separately, because an all-in-one `gh release create ... <large ISO>` can sit for a long time with no useful progress output.
