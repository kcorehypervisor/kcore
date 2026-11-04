# Air-gapped updates

Status: design. Nothing here is implemented yet.

An air-gapped site takes a kcore update the way it takes any other patch:

1. Someone downloads the release on a connected machine and verifies it.
2. The files cross the gap on the site's approved path: removable media, a
   data diode, or a transfer station that scans them.
3. Someone on the inside verifies them again and imports them into a local
   repository.
4. The update rolls out from that repository in a maintenance window. Nothing
   on the inside touches a network outside the site.

kcore ships one signed bundle per release and architecture for this. The
controller imports it and pushes what each node lacks. The update itself is
the normal `kctl update version set`. Switch, reboot, confirm, and rollback
already run without a network (see [cluster-updates.md](cluster-updates.md)).

## Scope

- **Community bundle: no compiled kcore.** The bundle carries the kcore
  source, the Rust crate sources, every flake input, and the nixpkgs outputs
  that `cache.nixos.org` already serves. Each node compiles the kcore packages
  during prepare, as Community nodes do on a connected site.
- **Subscriber bundle (later channel).** The same layout, plus prebuilt kcore
  closures, signed with a Nix key for that channel. It is out of scope here.
- **Generic by default.** One bundle serves every site. It does not need
  anything from the site, so nothing has to leave the air gap.
- **Full and delta bundles.** A full bundle is self-contained. A delta holds
  only what changed since an earlier release that the site already imported.

## What is in a bundle

Each release publishes these assets next to the ISO:

| Asset | Contents |
|---|---|
| `kcore-airgap-X.Y.Z-ARCH.manifest.json` | Version, commit, tag, `schemaVersion`, architecture, the reference configurations it covers, and every store path with its NAR SHA-256, size, and kind (`source` or `nixpkgs`). |
| `kcore-airgap-X.Y.Z-ARCH.tar` | `kcore.bundle` (a `git bundle` of the tag) and a Nix file binary cache (`nix-cache-info`, `*.narinfo`, `nar/*.nar.zst`) holding every path in the manifest. |
| `kcore-airgap-X.Y.Z-from-W.V.U-ARCH.tar` | Delta from the previous release: the git bundle and only the paths that are not in the previous manifest. |

`SHA256SUMS` lists all of them, and the existing `SHA256SUMS.sigstore.json`
signs it. The tar uses the layout Nix already reads (`nix copy --from
file://…`), so the import needs no custom store format.

## Trust

The bundle has one signature: the Sigstore keyless signature the release
already puts on `SHA256SUMS`. There is no new long-lived key.

The chain of trust:

1. `SHA256SUMS.sigstore.json` signs `SHA256SUMS`. It is checked against the
   release signer's identity and OIDC issuer.
2. `SHA256SUMS` gives the hash of the manifest and of each tar.
3. The manifest gives the NAR hash of every store path.
4. Nix checks every path as it imports it, with `require-sigs` left on:
   - nixpkgs outputs keep their `cache.nixos.org-1` signature, which NixOS
     trusts by default;
   - sources (the kcore tree, flake inputs, crate tarballs) are
     content-addressed, so Nix accepts them without a signature.

   A path that is neither is refused. A source's store path is derived from
   its content, and a node only uses the paths its lock and derivations name.
   A tampered bundle therefore cannot replace anything the release expects.

Checked on 2026-11-04 with cosign 3.0.5 and Nix:

- The v0.6.1 `SHA256SUMS.sigstore.json` verifies with no network
  (`unshare -rn cosign verify-blob --bundle … --trusted-root trusted_root.json
  …`), and a modified `SHA256SUMS` fails.
- A content-addressed source imports from an unsigned file cache into a fresh
  store. An input-addressed output without a trusted signature is refused.

### Verifying offline

A Sigstore bundle carries the certificate, the signature, and the
transparency-log inclusion proof. Verifying it needs Sigstore's trusted root
(the Fulcio CA, the Rekor and timestamp keys), not a connection to Rekor.

- **Outside:** `kctl update bundle verify` checks the signature online, the
  way `scripts/verify-release.sh` does today. It then writes the trusted root
  it used, fetched through Sigstore's TUF repository, next to the bundle as
  `trusted_root.json`. It prints that file's SHA-256.
- **Inside:** the controller verifies with `cosign verify-blob --trusted-root`
  from its own closure. It uses the trusted root pinned in the kcore release it
  runs, which arrived with an earlier verified release. A carried
  `trusted_root.json` is used only when the operator passes
  `--trusted-root-sha256 <hash>`, matching the value printed outside. That
  covers a Sigstore key rotation between two releases.

The expected signer identity (`82217+rtacconi@users.noreply.github.com`) and
OIDC issuer (`https://github.com/login/oauth`) are pinned in the release (`lib.kcore`), not
typed by the operator.

## Generic coverage

A node's system depends on its own files (hostname, hardware, `node.nix`) and
on the features it enables. The bundle covers that without seeing the site:

1. The release builds a set of reference node configurations: base (agent and
   controller), Ceph, ZFS, NVIDIA and vGPU, and ingress. It builds them in an
   empty store whose only substituter is `cache.nixos.org`.
2. Every path that the build fetches from `cache.nixos.org`, and every
   fixed-output path it downloads (sources, crates), goes into the bundle.
3. Paths built locally (kcore packages, units, `/etc`, the initrd, the
   toplevel) do not go in. A node rebuilds them from what the bundle holds.
4. The release then rehearses an air-gapped node. It imports the bundle into
   a second empty store with the network off and substituters disabled, and
   builds each reference configuration. The release fails if any build needs
   something the bundle lacks.

A site that enables something outside the reference set finds out from
`kctl update bundle check` (below), before the maintenance window. The fix is
a supplement made outside: `kctl update bundle create --supplement <bundle>
--add <installable>…`. It holds only nixpkgs outputs and sources, so Nix
verifies it the same way and it needs no kcore signature.

## Delta bundles

A delta is a subset of a full bundle. It carries the full release's signed
manifest and only the paths that are not in an earlier manifest.

- The release publishes a delta from the previous release.
- `kctl update bundle create --from W.V.U X.Y.Z` makes any other delta on the
  connected side. It verifies both manifests and the full bundle, then cuts
  the subset. Anyone can make a delta: its paths are checked against the
  release's own signed manifest, so it needs no new signature.
- On import the controller checks that every path in the manifest is either
  in the delta or already in its bundle repository from an earlier import.
  If not, it refuses the delta and lists what is missing.

## Inside the site

```bash
kctl update bundle import kcore-airgap-0.7.0-x86_64.tar   # or the delta
kctl update bundle check v0.7.0
kctl update version set v0.7.0
```

**Import.** `kctl` streams the tar, the manifest, `SHA256SUMS`, and its
signature to the controller. The controller then:

1. verifies the signature, the manifest hash, and every NAR hash before it
   keeps anything;
2. adds the tag from `kcore.bundle` to a git mirror at
   `/var/lib/kcore/mirror/kcore.git`;
3. stores the paths in `/var/lib/kcore/bundles/`;
4. records who imported which bundle, when, and with which hashes.

`kctl update bundle list` shows imported bundles. `kctl update bundle remove`
deletes one. The bundle a node runs, and the one before it, cannot be removed,
so a downgrade also works offline.

**Check.** The controller asks each node which store paths its config needs
for the target release that it does not have and the bundles cannot supply.
The node evaluates its staged host flake and runs an offline dry-run build.
The command prints, per node, either `complete` or the missing paths.

**Rollout.** `version set` resolves the tag against the controller's mirror
when a bundle for it is imported, so `--flake-ref` is not needed. During
prepare the controller pushes each node the paths it lacks over the existing
mTLS connection. The node agent writes them to a temporary file cache and
imports them with `nix copy --from file://…`, under the node's normal
signature policy. Prepare then builds with substituters disabled. A missing
path fails prepare, which changes nothing on the node. The node pins the
imported paths with a GC root until the update finishes.

Nodes never pull from the controller, and no substituter is configured. That
keeps the release rule against shipping a binary cache in the Community
release.

## Media and scanning

- The tar is uncompressed at the outer level, so scanners can list it. The
  NARs inside are zstd-compressed Nix archives.
- `kctl update bundle extract --to <dir>` unpacks every store path into
  plain directories, for scanners that need to read file contents. The import
  still uses the original tar.
- GitHub limits a release asset to 2 GiB. A bundle above that is published as
  numbered parts (`.tar.000`, `.tar.001`, …), each listed in `SHA256SUMS`.
  `kctl update bundle import` accepts the first part and reads the rest.
- Each import is a single file, or a short numbered sequence. Neither the
  import nor the scan runs anything from the bundle.

## Changes by component

| Component | Change |
|---|---|
| Flake and release | `airgapBundle.<arch>` output: reference builds, path collection, rehearsal, tar and manifest. `scripts/release.sh` adds the assets to `dist/` before `SHA256SUMS` is signed. Update `docs/release.md`, `docs/release-verification.md`, and the release-train rule's asset list. |
| `lib.kcore` | Pinned signer identity, OIDC issuer, and Sigstore trusted root. |
| kctl | `update bundle verify`, `create` (deltas and supplements), `import`, `check`, `list`, `remove`, `extract`. |
| Controller | Bundle repository, local git mirror, cosign verification, import audit record, streaming import RPC, mirror as default flake ref, push during prepare. |
| Node agent | Streaming store-path import RPC, offline prepare, dry-run check RPC, GC roots for imported paths. |
| Tests | Unit tests for manifest and delta checks. A CI check with a small reference configuration that runs the full collect, import, offline-build cycle. The full rehearsal runs at release time, because a full bundle is several GB. |

## Delivery order

1. Bundle builder and manifest, with the offline rehearsal, as a flake output.
2. Controller import and verification, the git mirror, and `kctl update bundle
   verify | import | list | remove`.
3. Node import, offline prepare, and `kctl update bundle check`.
4. Delta and supplement bundles, and the published delta asset.
5. End-to-end test on a lab node with all outbound traffic blocked.

## Open points

- **Size.** Measure the full bundle and the typical delta at step 1. That
  decides whether parts are needed from the first release.
- **Reference set.** Confirm which features the reference configurations
  cover.