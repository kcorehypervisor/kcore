# Caddy with the plugins the ingress gateway renders configuration for.
#
# Fixed-output derivation: Community nodes compile it like the rest of the
# flake and no binary cache is involved. Refresh `hash` in the same commit
# that bumps a pin and the VERSION that ships it (docs/release.md).
{ pkgs }:
pkgs.caddy.withPlugins {
  # coraza-caddy v2.6.x and caddy-l4 v0.1.1+ require a newer Caddy than the
  # 2.11.2 in nixpkgs, so the last releases that build against it are pinned. Bump together with nixpkgs.
  plugins = [
    "github.com/corazawaf/coraza-caddy/v2@v2.5.0"
    "github.com/mholt/caddy-ratelimit@v0.1.0"
    "github.com/mholt/caddy-l4@v0.1.0"
    "github.com/caddy-dns/cloudflare@v0.2.4"
    "github.com/caddy-dns/route53@v1.6.2"
    "github.com/caddy-dns/rfc2136@v1.0.0"
  ];
  hash = "sha256-/X6JueRE7pbuunh4qChbqBfinWYO3COKt9hF9Q99I5c=";
}
