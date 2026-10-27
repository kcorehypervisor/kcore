# Gateway end to end: a Pebble ACME server issues the certificate, one backend
# answers, and Coraza (OWASP CRS, blocking) refuses an XSS probe.
#
# Needs `modules/kcore-ingress-caddy.nix` to carry its real fixed-output hash.
{ pkgs, ... }:
let
  pebbleCerts = "${pkgs.pebble.src}/test/certs";
  host = "shop.test";

  wafDirectives = ''
    Include @coraza.conf-recommended
    Include @crs-setup.conf.example
    Include @owasp_crs/*.conf
    SecRuleEngine On
  '';

  caddyConfig = pkgs.writeText "kcore-ingress-test.json" (
    builtins.toJSON {
      admin.listen = "unix//run/kcore-ingress/admin.sock|0600";
      storage = {
        module = "file_system";
        root = "/var/lib/kcore/ingress/certmagic";
      };
      apps = {
        http.servers.kcore = {
          listen = [
            ":80"
            ":443"
            # Not assigned to any interface. The module's nonlocal-bind sysctl
            # is what makes this succeed; a VIP works the same way.
            "192.0.2.10:8443"
          ];
          routes = [
            {
              match = [ { host = [ host ]; } ];
              handle = [
                {
                  handler = "waf";
                  load_owasp_crs = true;
                  directives = wafDirectives;
                }
                {
                  handler = "reverse_proxy";
                  upstreams = [ { dial = "127.0.0.1:8080"; } ];
                }
              ];
            }
          ];
        };
        tls.automation.policies = [
          {
            subjects = [ host ];
            issuers = [
              {
                module = "acme";
                ca = "https://localhost:14000/dir";
                email = "ops@${host}";
                trusted_roots_pem_files = [ "${pebbleCerts}/pebble.minica.pem" ];
              }
            ];
          }
        ];
      };
    }
  );

  pebbleConfig = pkgs.writeText "pebble.json" (
    builtins.toJSON {
      pebble = {
        listenAddress = "0.0.0.0:14000";
        managementListenAddress = "0.0.0.0:15000";
        certificate = "${pebbleCerts}/localhost/cert.pem";
        privateKey = "${pebbleCerts}/localhost/key.pem";
        httpPort = 80;
        tlsPort = 443;
        ocspResponderURL = "";
        externalAccountBindingRequired = false;
      };
    }
  );
in
pkgs.testers.runNixOSTest {
  name = "kcore-ingress";

  nodes.gateway =
    { pkgs, ... }:
    {
      imports = [ ../modules/kcore-ingress.nix ];

      networking.extraHosts = "127.0.0.1 ${host} localhost";
      kcore.ingress = {
        enable = true;
        listenAddresses = [ "127.0.0.1" ];
      };

      systemd.services.pebble = {
        wantedBy = [ "multi-user.target" ];
        before = [ "kcore-ingress.service" ];
        environment = {
          PEBBLE_VA_NOSLEEP = "1";
          PEBBLE_WFE_NONCEREJECT = "0";
        };
        serviceConfig.ExecStart = "${pkgs.pebble}/bin/pebble -config ${pebbleConfig}";
      };

      systemd.services.backend = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig.ExecStart = "${pkgs.python3}/bin/python3 -m http.server 8080 --bind 127.0.0.1 --directory ${pkgs.writeTextDir "index.html" "kcore-backend\n"}";
      };

      environment.systemPackages = [
        pkgs.curl
        pkgs.openssl
      ];
    };

  testScript = ''
    gateway.wait_for_unit("pebble.service")
    gateway.wait_for_unit("backend.service")
    gateway.wait_for_unit("kcore-ingress.service")
    # The unit boots a bootstrap config that only opens the admin socket.
    # Port 80 exists after the configuration below is loaded.
    gateway.wait_for_open_port(14000)
    gateway.wait_for_file("/run/kcore-ingress/admin.sock")
    gateway.succeed("sysctl -n net.ipv4.ip_nonlocal_bind | grep -q 1")

    # Push the rendered configuration the way the node agent does.
    gateway.succeed(
        "curl -fsS --unix-socket /run/kcore-ingress/admin.sock "
        "-X POST -H 'Content-Type: application/json' "
        "--data-binary @${caddyConfig} http://localhost/load"
    )

    gateway.wait_until_succeeds("ss -H -ltn sport = :8443 | grep -q 192.0.2.10")

    # Pebble 2.x generates its issuance root at startup. The minica file only
    # authenticates the ACME and management listeners.
    gateway.succeed(
        "curl -fsS --cacert ${pebbleCerts}/pebble.minica.pem "
        "https://localhost:15000/roots/0 -o /tmp/pebble-root.pem"
    )
    gateway.wait_until_succeeds(
        "curl -fsS --cacert /tmp/pebble-root.pem "
        "--resolve ${host}:443:127.0.0.1 https://${host}/ | grep -q kcore-backend",
        timeout=120,
    )

    # The served chain is the one ACME issued, not Caddy's internal issuer.
    gateway.succeed(
        "echo | openssl s_client -connect 127.0.0.1:443 -servername ${host} 2>/dev/null "
        "| openssl x509 -noout -issuer | grep -qi pebble"
    )

    code = gateway.succeed(
        "curl -ks -o /dev/null -w '%{http_code}' --resolve ${host}:443:127.0.0.1 "
        "'https://${host}/?q=<script>alert(1)</script>'"
    ).strip()
    assert code == "403", f"expected 403 for XSS probe, got {code}"
  '';
}
