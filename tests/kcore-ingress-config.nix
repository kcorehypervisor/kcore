# Loads the controller's golden Caddy configurations in the real Caddy build.
# The goldens come from `ingress_render.rs`; `caddy validate` provisions every
# module they name (Coraza, rate limit, layer 4, DNS-01, inline CA pools), so a
# renamed field or a plugin dropped from the build fails here and not on a node.
{
  pkgs,
  caddy,
  fixtures,
}:
pkgs.runCommand "kcore-ingress-config"
  {
    nativeBuildInputs = [ caddy ];
  }
  ''
    export HOME="$TMPDIR/home"
    mkdir -p "$HOME"
    # The goldens use the node paths; point them at the sandbox.
    for name in caddy-shop caddy-multi; do
      sed \
        -e "s#/var/log/kcore/ingress/waf.jsonl#$TMPDIR/waf.jsonl#g" \
        -e "s#/var/lib/kcore/ingress/certmagic#$TMPDIR/certmagic#g" \
        -e "s#unix//run/kcore-ingress/admin.sock|0600#unix/$TMPDIR/admin.sock|0600#" \
        ${fixtures}/$name.golden.json > "$TMPDIR/$name.json"
      # A well-formed token: the Cloudflare provider checks the shape.
      CLOUDFLARE_API_TOKEN=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA \
        caddy validate --config "$TMPDIR/$name.json"
    done
    touch $out
  ''
