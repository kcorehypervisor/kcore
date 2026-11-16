# Mail server

`kcore.mail` turns a kcoreOS host, or a NixOS VM on kcore, into a mail server:
SMTP in and out, IMAP, JMAP and ManageSieve, with spam filtering, DKIM, SPF,
DMARC and a web admin. It runs [Stalwart](https://stalw.art) from the nixpkgs
kcore already pins, behind nginx for HTTPS.

Run it in a VM rather than on a hypervisor node: a mail server wants its own
public IP, PTR record and reputation, and should not share a failure domain
with the cluster it is hosted on.

## Architecture

```text
                        Internet
                           │
     ┌─────────────────────┼──────────────────────────────┐
     │ :25 :465 :587 :993  │ :80 :443                     │
     │ (:4190 optional)    │                              │
     │        ▼            ▼                              │
     │  ┌───────────┐   ┌──────────────┐  ACME HTTP-01    │
     │  │ Stalwart  │◄──┤    nginx     │◄── Let's Encrypt │
     │  │ SMTP IMAP │   │ TLS, HSTS,   │                  │
     │  │ JMAP spam │   │ proxy        │                  │
     │  │ DKIM      │   └──────┬───────┘                  │
     │  │           │◄─────────┘ http://127.0.0.1:8080    │
     │  └─────┬─────┘     (web admin, JMAP, API)          │
     │        │ reads                                     │
     │        ▼                                           │
     │  /var/lib/acme/<hostname>/{fullchain,key}.pem      │
     │  <dataDir>/db  (RocksDB: mail, index, directory)   │
     └────────────────────────────────────────────────────┘
```

- **One mail process.** Stalwart replaces the Postfix, Dovecot, Rspamd and
  OpenDKIM stack. It is `services.stalwart` from the pinned nixpkgs, so a
  nixpkgs bump upgrades it with the rest of kcoreOS and no extra flake input is
  involved.
- **nginx owns HTTPS.** nginx terminates TLS for the web admin and JMAP and
  proxies to Stalwart's HTTP listener, which binds to loopback only. Stalwart
  is told the public URL (`http.url`) and trusts `X-Forwarded-For`
  (`http.use-x-forwarded`), so links and client addresses are right.
- **One certificate.** nginx obtains the Let's Encrypt certificate for
  `hostname` (and `extraHostnames`). Stalwart is in the certificate's group
  and reads the same files for STARTTLS and implicit TLS on the mail ports.
- **Storage.** Everything (messages, full-text index, directory) is in one
  RocksDB under Stalwart's data directory. Backups cover that one directory.

## What Nix owns and what the web admin owns

Stalwart reads settings from its TOML file and from its database. A key is
*local* when it is read only from the file. The module sets
`config.local-keys` to Stalwart's built-in list plus `http.*`, so every key it
writes is local and the web admin cannot shadow it from the database.

| Owned by Nix (`kcore.mail`) | Owned by the web admin (database) |
|---|---|
| Hostname, listeners and ports | Domains |
| Certificate paths | Accounts, groups, aliases, mailing lists |
| Public URL and proxy trust | DKIM keys |
| Storage backend | Spam filter tuning, Sieve scripts |
| Fallback admin (`admin`) | Queue, reports, everything else |

Rebuilding the host never touches mailboxes, accounts or DKIM keys.

## Ports

| Port | Protocol | Notes |
|------|----------|-------|
| 25 | SMTP | Mail from other servers. The provider must allow it outbound too. |
| 465 | SMTP submission, implicit TLS | Clients |
| 587 | SMTP submission, STARTTLS | Clients |
| 993 | IMAP, implicit TLS | Plain IMAP (143) is not offered. |
| 4190 | ManageSieve | Only with `manageSieve.enable`. |
| 80 | HTTP | ACME HTTP-01 and redirect to HTTPS |
| 443 | HTTPS | Web admin, JMAP, management API |
| 8080 | HTTP | Loopback only, never opened in the firewall (`httpPort`) |

## NixOS module

[`modules/kcore-mail.nix`](../modules/kcore-mail.nix), exported as
`nixosModules.kcore-mail`:

```nix
kcore.mail = {
  enable = true;
  hostname = "mail.example.com";
  extraHostnames = [ "autoconfig.example.com" "mta-sts.example.com" ];
  acme = {
    email = "postmaster@example.com";
    staging = true; # first deployment only
  };
  adminSecretFile = config.age.secrets.stalwart-admin.path;
  manageSieve.enable = false;
};
```

| Option | Default | Meaning |
|--------|---------|---------|
| `hostname` | none | SMTP banner, certificate and web admin URL. Must match the PTR record. |
| `extraHostnames` | `[ ]` | More names on the same vhost and certificate. Each needs DNS first. |
| `acme.email` | none | Let's Encrypt account contact. |
| `acme.staging` | `false` | Use the Let's Encrypt staging directory. |
| `adminSecretFile` | none | Password, or a SHA-512 crypt / bcrypt / Argon2 hash, for `admin`. Must be outside the Nix store; passed as a systemd credential. |
| `httpPort` | `8080` | Loopback port nginx proxies to. |
| `manageSieve.enable` | `false` | Open ManageSieve on 4190. |

Anything else Stalwart supports can be set through `services.stalwart.settings`.
Keys outside the local list then live in both places; add them to
`services.stalwart.settings.config.local-keys` to keep Nix authoritative.

The data directory follows `system.stateVersion`: `/var/lib/stalwart-mail`
before 26.05 and `/var/lib/stalwart` from 26.05 on (as does the service user).
Changing `stateVersion` on an existing server does not move the data.

## Certificate lifecycle

1. `acme-<hostname>.service` writes a self-signed placeholder at first boot.
   Stalwart is ordered after it, so it always has a certificate to load.
2. `acme-order-renew-<hostname>.service` orders the real certificate over
   HTTP-01 through nginx.
3. Each renewal reloads nginx and restarts Stalwart (it has no reload). The
   restart drops open IMAP sessions; clients reconnect.

## First deployment

1. **Pre-flight.** Confirm outbound port 25 is open, set the PTR record of
   every address (IPv4, and IPv6 if used) to `hostname`, and check the IP is
   not on a blocklist.
2. **DNS for the host.** `A`/`AAAA` for `hostname` and each of
   `extraHostnames`.
3. **Deploy with `acme.staging = true`.** Check `systemctl status stalwart
   nginx`, the listening ports, and the web admin at `https://<hostname>`.
4. **Switch to production** (`acme.staging = false`) and rebuild.
5. **Create the domain** in the web admin as `admin`, and create its DKIM
   keys (web admin, or `POST /api/dkim`). The web admin then lists the DNS
   records the domain needs (`/api/dns/records/<domain>`).
6. **Publish mail DNS**, then create accounts.

| Type | Name | Value |
|------|------|-------|
| MX | `example.com` | `10 mail.example.com` |
| TXT | `example.com` | `v=spf1 mx -all` |
| TXT | `<selector>._domainkey.example.com` | From the web admin |
| TXT | `_dmarc.example.com` | `v=DMARC1; p=none; rua=mailto:dmarc@example.com`, then `p=quarantine` once reports are clean |
| TXT | `_mta-sts`, `_smtp._tls` | Optional; Stalwart serves the MTA-STS policy on `mta-sts.<domain>` |

Verify with an external mailbox: the `Authentication-Results` header should
show SPF, DKIM and DMARC as `pass`.

## Backups

Back up Stalwart's data directory. It holds mail, accounts and the DKIM keys;
losing the keys means republishing DKIM records. RocksDB files should be
copied from a filesystem snapshot (ZFS, LVM or the VM disk), or with Stalwart
stopped, not while it writes.

## Licence

Stalwart is AGPL-3.0-only; the `stalwart-enterprise` build adds the
proprietary SELv1 licence and is unfree. kcore uses the unmodified `stalwart`
package and only configures it, so kcore's own licence is unaffected. Do not
link Stalwart crates into kcore binaries: that would make the combined program
AGPL. Patching Stalwart for a hosted service obliges offering that patched
source to its users.

## Tests

| Check | What it covers | Where it runs |
|-------|----------------|---------------|
| `checks.x86_64-linux.mail-module-eval` | Listeners, local keys, certificate paths, nginx vhost, firewall, ManageSieve toggle, disabled module | CI |
| `checks.x86_64-linux.kcore-mail` | One VM: placeholder certificate, nginx proxy and HSTS, domain and accounts created over the API, a message sent on 587 and read on 993, mail ports serving the ACME certificate | On demand; needs a host that builds NixOS VMs |

```bash
nix build -L .#checks.x86_64-linux.kcore-mail
```

## Out of scope (v1)

- Declarative domains and accounts (they live in Stalwart's directory)
- Webmail (Stalwart has none; Roundcube or similar can sit behind the same nginx)
- Clustered Stalwart (FoundationDB or PostgreSQL storage, several nodes)
- Controller or `kctl` integration
- The Stalwart enterprise build
