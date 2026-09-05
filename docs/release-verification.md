# Verifying a kcore Release

Every release publishes a checksum manifest, a Sigstore signature over that
manifest, and two SBOMs. This document is the procedure for checking them.
[`scripts/verify-release.sh`](../scripts/verify-release.sh) implements exactly
these steps, so the documented path and the executable path cannot drift.

For how releases are produced, see [release.md](./release.md). For why
Sigstore rather than the cluster PKI, see the "Release Artifacts" section of
[security.md](./security.md).

## What a release contains

For version `X.Y.Z` (tag `vX.Y.Z`):

| Asset | Contents |
|---|---|
| `kcoreos-X.Y.Z-x86_64-linux.iso` | Installer ISO (~1.3 GB) |
| `kctl-X.Y.Z-linux-x86_64.tar.gz` | Linux `kctl`, Nix-built |
| `kctl-X.Y.Z-macos-x86_64.tar.gz` | Intel macOS `kctl` |
| `kctl-X.Y.Z-macos-aarch64.tar.gz` | Apple Silicon `kctl` |
| `kcore-X.Y.Z-crates.cdx.json` | CycloneDX SBOM of the Rust dependency graph |
| `kcore-X.Y.Z-iso-closure.cdx.json` | CycloneDX SBOM of the ISO's Nix closure |
| `kcore-X.Y.Z-iso-closure.spdx.json` | The same closure, in SPDX |
| `SHA256SUMS` | SHA-256 of every asset above |
| `SHA256SUMS.sigstore.json` | Sigstore bundle signing `SHA256SUMS` |

Only `SHA256SUMS` is signed. Because it lists every other asset, verifying
that one signature and then checking hashes against the manifest covers the
whole release, so there is exactly one signature to reason about.

## What the signature proves

**It proves a kcore maintainer stood behind these exact bytes.** Verification
succeeds only if the bundle was produced by the identity you name on the
command line, and only if the bytes hash to what that identity signed.

**It does not prove the artifacts were built from any particular commit.**
Releases are cut on a maintainer's own Linux machine, not by a pipeline, so
nothing in the release records which source revision or toolchain produced a
given artifact. Do not read the signature as a statement about how the bytes
came to exist; read it as a statement about who is accountable for them.

What it does replace is a weaker check. Without a signature, `SHA256SUMS` is
downloaded from the same host as the artifacts it describes, so it detects
corruption in transit but not substitution: whoever can replace the tarball
can replace the manifest to match. The signature is what makes the manifest
worth trusting.

## Prerequisites

- **`cosign` 3.0.5 or newer.** Sigstore is mid-migration to Rekor v2.
  cosign 3.0.0–3.0.4 fails to verify Rekor v2 bundles unless you also pass
  `--use-signed-timestamps`; 3.0.5 handles it automatically and defaults to
  the `.sigstore.json` bundle format this release uses.
  `scripts/verify-release.sh` refuses to run on anything older. From a
  checkout, `nix develop --command cosign version` gives the pinned build.
- **Network access to Rekor.** Verification checks the bundle's inclusion
  proof against the public transparency log at `rekor.sigstore.dev`. This is
  not an offline check.
- **`gh`** and **`sha256sum`** (or `shasum -a 256` on macOS), for the manual
  path below.

## The expected signing identity

A Sigstore signature is only meaningful against an identity you expect.
Verifying that *somebody* signed the bytes proves nothing at all, so both
the script and the commands below require the identity to be named
explicitly.

| Field | Value |
|---|---|
| `--certificate-identity` | *(not yet published — see below)* |
| `--certificate-oidc-issuer` | `https://oauth2.sigstore.dev/auth` |

The issuer is Sigstore's own OIDC provider, which is what appears in the
certificate when a maintainer signs interactively from their machine. The
identity is the **email address** of the account chosen at that prompt, not a
GitHub username or URL.

> **This table is incomplete until the first signed release.** No kcore
> release has been signed yet, so there is no identity to publish. The
> maintainer who cuts the first signed release must record the value here,
> and set `SIGNER_IDENTITY` in [`scripts/verify-release.sh`](../scripts/verify-release.sh)
> and `DEFAULT_SIGNER_IDENTITY` in [`scripts/get-kctl.sh`](../scripts/get-kctl.sh).
> Until then, pass the identity yourself with `KCORE_SIGNER_IDENTITY`.

To read the identity out of a bundle you already have — useful for
confirming what was published:

```bash
cosign verify-blob \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp '.*' \
  --certificate-oidc-issuer-regexp '.*' \
  SHA256SUMS
```

That tells you which identity signed, but it is **not** verification: it
accepts any identity, which is the whole thing you are trying to pin down.
Use it once to learn the value, then verify against it explicitly.

## Verifying (the short way)

```bash
make verify-release V=v0.3.0
```

The target runs [`scripts/verify-release.sh`](../scripts/verify-release.sh),
which downloads the release assets, checks the `cosign` version, verifies the
signature over `SHA256SUMS`, then checks every downloaded file against it.

The ISO is skipped by default, since verifying the manifest first and then
fetching only what you need is the point of signing a manifest. Set
`KCORE_VERIFY_ISO=1` to include it, or name asset patterns:

```bash
KCORE_VERIFY_ISO=1 make verify-release V=v0.3.0
bash scripts/verify-release.sh v0.3.0 'kctl-*-linux-x86_64.tar.gz'
```

Useful environment variables:

| Variable | Effect |
|---|---|
| `KCORE_SIGNER_IDENTITY` | Expected certificate identity (exact match) |
| `KCORE_SIGNER_IDENTITY_REGEXP` | Regexp alternative, when several maintainers may sign |
| `KCORE_SIGNER_ISSUER` | Expected OIDC issuer |
| `KCORE_VERIFY_DIR` | Download directory; kept on exit instead of a temp dir |
| `KCORE_VERIFY_ISO` | `1` also downloads and checks the ISO |

## Verifying (by hand)

The same three steps, for an auditor who would rather not run our script.

**1. Download the manifest, the signature, and whatever you want to check.**

```bash
gh release download v0.3.0 --repo kcorehypervisor/kcore \
  --pattern 'SHA256SUMS' \
  --pattern 'SHA256SUMS.sigstore.json' \
  --pattern 'kctl-*-linux-x86_64.tar.gz' \
  --pattern '*.cdx.json' \
  --pattern '*.spdx.json'
```

**2. Verify the signature over the manifest.**

```bash
cosign verify-blob \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-identity 'MAINTAINER_EMAIL' \
  --certificate-oidc-issuer 'https://oauth2.sigstore.dev/auth' \
  SHA256SUMS
```

Expect `Verified OK`. Anything else means stop — do not fall back to
checking hashes on their own, because an unverified manifest is exactly what
the signature exists to replace.

**3. Check the artifacts against the now-trusted manifest.**

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

`--ignore-missing` is what lets you check a partial download: without it,
`sha256sum` fails on every asset you chose not to fetch. On macOS use
`shasum -a 256 --check --ignore-missing SHA256SUMS`.

## Sigstore properties worth understanding

**There is no revocation.** Fulcio certificates are valid for roughly ten
minutes — long enough to sign, and then expired. Nothing to revoke afterwards
exists, and Rekor is an append-only, timestamped log, so entries cannot be
withdrawn. Verification succeeds after expiry precisely because the log entry
proves the signature was made while the certificate was valid.

The practical consequence: if a maintainer's identity is compromised, the
response is not revocation. It is to read the Rekor log, enumerate everything
signed by that identity inside the compromise window, and publicly repudiate
those specific artifacts. The timestamped log is what makes that possible;
this is a different incident-response shape from a CRL, not a missing
feature.

**Rotation is free.** There is no key to migrate. A new maintainer, or a
maintainer with a new address, signs with their own identity and the expected
`--certificate-identity` published above changes. Old releases keep verifying
against the old identity because their log entries are unchanged, so
`SIGNER_IDENTITY` should be treated as per-release information rather than a
single global constant once it has ever changed.

**Verification is online.** Checking the inclusion proof requires reaching
Rekor. There is no supported way to verify these bundles fully offline, so an
air-gapped consumer must verify on a connected host and carry the artifacts
across.

**Signing is public.** The certificate, including the maintainer's email
address, is written to a public append-only log. That is inherent to
Sigstore's transparency model and cannot be undone after the fact.

## The SBOMs

The two SBOMs describe different graphs and neither contains the other.

`kcore-X.Y.Z-crates.cdx.json` is the Rust side, generated with
`cargo-cyclonedx` and merged with `cyclonedx-cli`. It is a **hierarchical**
CycloneDX document: `kcore X.Y.Z` at the top, one sub-component per workspace
member, each carrying its own resolved dependency subtree. It is shaped that
way on purpose — kcore ships several binaries with different dependency sets,
and a flat union would assert one dependency list that no single shipped
binary actually has. The hierarchy lets you ask which binary pulls in a given
crate, not merely whether kcore contains it somewhere.

`kcore-X.Y.Z-iso-closure.cdx.json` and `.spdx.json` are the same Nix closure
in both formats, generated by `sbomnix`. This is where the kernel, systemd,
`cloud-hypervisor` and openssl appear; the Cargo graph knows nothing about
them.

The target is the flake reference
`.#nixosConfigurations.kcore-iso.config.system.build.toplevel` — the NixOS
system the ISO boots and installs, roughly 589 store paths. It is
deliberately **not** `.build.isoImage`: an ISO derivation outputs a single
opaque squashfs blob with no store references, so its runtime closure is
empty. `scripts/sbom.sh` enforces a component floor for exactly this reason,
because an empty SBOM is still schema-valid.

Both are covered by `SHA256SUMS`, so the verification above applies to them
as with any other asset. Neither is committed to the repository: they are
generated at release time, which means there is no stored copy to fall out of
date.

## Installing `kctl` with verification

[`scripts/get-kctl.sh`](../scripts/get-kctl.sh) verifies the signature
automatically **when `cosign` is already on `PATH`** and a signer identity is
configured, and otherwise falls back silently to checksums alone. This keeps
`curl … | bash` working for users without `cosign`, at the cost of it being
best-effort: it is not a substitute for verifying deliberately.

```bash
# Optional but recommended: install cosign first, then
curl -fsSL https://raw.githubusercontent.com/kcorehypervisor/kcore/main/scripts/get-kctl.sh | bash
```

Skipping happens quietly if `cosign` is absent, older than 3.0.5, or the
release carries no signature. A signature that is present and **invalid**
aborts the install.
