# Enterprise update channel

Paying subscribers receive each release **14 days before** the public tag and
ISO, with **prebuilt** kcore packages so nodes download instead of compiling.

Access is GitHub-native: the private repository
[`kcorehypervisor/enterprise`](https://github.com/kcorehypervisor/enterprise)
holds signed release assets. Customers are read-only collaborators; their
cluster stores a classic GitHub personal access token and uses it only to
fetch those assets.

Community Edition is unchanged: nodes still build from
`github:kcorehypervisor/kcore` and never configure a binary cache.

## What a release contains

| Asset | Where |
|---|---|
| `kcore-prebuilt-X.Y.Z-x86_64-linux.tar` | Enterprise only. Nix file binary cache of kcore packages signed with `kcore-enterprise-1`. |
| `kcore-X.Y.Z-release.json` | Enterprise: `"channel":"enterprise"`, `"nodeBuild":"prebuilt"`, `kcoreRev`, `prebuilt`. |
| ISO and `kctl` archives | Enterprise during the embargo; community after day 14. |
| `SHA256SUMS` + Sigstore bundle | Both channels. |

Source stays public. Enterprise nodes pin
`github:kcorehypervisor/kcore/<commit>` so they need no tag and no token to
fetch source. The public tag is pushed on day 14.

## Customer setup

1. Create a **dedicated** GitHub account used only for kcore updates (a classic
   `repo` token sees every private repo that account can access).
2. Ask support to add that account as a read-only collaborator on
   `kcorehypervisor/enterprise`.
3. On that account, create a **classic** personal access token with the
   `repo` scope. Fine-grained tokens cannot access an outside-collaborator
   org repository.
4. On the cluster:

```bash
kctl set subscription --token-file ./github-token
kctl get subscription
kctl update version set vX.Y.Z          # defaults to enterprise when a subscription is stored
kctl update version set vX.Y.Z --channel community   # opt out for one update
```

5. When the subscription ends, support removes the collaborator. Clear the
   cluster with `kctl set subscription --clear`.

## Operator onboarding and offboarding

```bash
# After payment / renewal
./scripts/enterprise-access.sh add <github-user>

# After cancellation or non-renewal
./scripts/enterprise-access.sh remove <github-user>
./scripts/enterprise-access.sh list
```

Reconcile the collaborator list against Stripe. The list is the access list.

## Maintainer release flow

Day 0 (after the version bump is on `main`):

```bash
make release    # local tag, build, dist, prebuilt, publish-enterprise, stage-public draft
```

Requires `KCORE_NIX_SIGNING_KEY_FILE` in `.env` pointing at the
`kcore-enterprise-1` secret key (never commit it). Cosign opens the browser
twice: once for the enterprise `SHA256SUMS`, once for the community draft.

Day 14:

```bash
make release-public
```

That pushes the public tag and publishes the draft. Pass
`RELEASE_FLAGS=--break-embargo` only for an urgent public fix.

Individual steps: `make release-prebuilt`, `make release-publish-enterprise`,
`make release-stage-public`, `make release-public`.

## How prepare uses the prebuilt cache

1. The controller reads the enterprise `release.json` and `SHA256SUMS` with
   the stored token (curl, token on stdin).
2. It starts a `ClusterUpdate` with `channel=enterprise`, the public
   `flake_rev`, and the prebuilt URL and SHA-256.
3. Each node downloads the tar, checks the SHA-256, unpacks it, and runs
   `nix build` with `--extra-substituters file://…` and
   `--extra-trusted-public-keys kcore-enterprise-1:…` for that build only.
4. No binary cache is written into the NixOS system configuration.

## Trust

- Sigstore keyless signature over each channel's `SHA256SUMS` (same signer
  identity as community releases).
- Nix signatures on prebuilt store paths with `kcore-enterprise-1`. The
  public key is compiled into the node agent.
- Public source at `kcoreRev` is the same tree community will get on day 14.

## Limits

- Prebuilt covers the flake packages a host update substitutes (`kctl`,
  `kcore-node-agent`, `kcore-controller`, `kcore-console`,
  `kcore-dashboard`). Features outside them (for example NVIDIA host
  drivers) still build on the node.
- The first release that ships this code goes to both channels the same day
  so clusters can update into the new controller and agent. The 14-day lead
  starts with the release after that.
- Collaborators on a private repo can see who watches or stars it. A
  dedicated, neutrally named GitHub account avoids that.
