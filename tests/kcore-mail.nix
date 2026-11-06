# Mail server end to end on one VM: Stalwart starts on the ACME self-signed
# placeholder (there is no CA to order from), nginx proxies the web admin, the
# fallback admin creates a domain and two accounts over the API, and a message
# sent on submission is read back over IMAPS.
# Not part of the fast nix-checks job; run it on a machine that can build
# NixOS VMs.
{ pkgs }:
let
  host = "mail.example.test";
  domain = "example.test";
  adminPassword = "kcore-mail-test-admin";
in
pkgs.testers.runNixOSTest {
  name = "kcore-mail";

  nodes.mail =
    { ... }:
    {
      imports = [ ../modules/kcore-mail.nix ];
      networking.hosts."127.0.0.1" = [ host ];
      virtualisation.memorySize = 2048;

      # A test fixture; real hosts point this at an agenix or sops secret.
      environment.etc."stalwart-admin".text = adminPassword;

      kcore.mail = {
        enable = true;
        hostname = host;
        acme.email = "postmaster@${domain}";
        adminSecretFile = "/etc/stalwart-admin";
      };

      environment.systemPackages = [
        pkgs.curl
        pkgs.openssl
        (pkgs.writers.writePython3Bin "mail-roundtrip" { } ''
          import imaplib
          import smtplib
          import ssl
          import time

          ctx = ssl.create_default_context()
          ctx.check_hostname = False
          ctx.verify_mode = ssl.CERT_NONE

          with smtplib.SMTP("${host}", 587) as smtp:
              smtp.starttls(context=ctx)
              smtp.login("alice", "alice-secret")
              smtp.sendmail(
                  "alice@${domain}",
                  "bob@${domain}",
                  "From: alice@${domain}\r\n"
                  "To: bob@${domain}\r\n"
                  "Subject: kcore-mail\r\n\r\n"
                  "hello from kcore-mail\r\n",
              )

          for _ in range(30):
              with imaplib.IMAP4_SSL("${host}", 993, ssl_context=ctx) as imap:
                  imap.login("bob", "bob-secret")
                  imap.select()
                  _, [ids] = imap.search(None, "ALL")
                  if ids:
                      _, msg = imap.fetch(ids.split()[-1], "BODY[TEXT]")
                      assert b"hello from kcore-mail" in msg[0][1], msg
                      break
              time.sleep(1)
          else:
              raise SystemExit("message never reached bob's inbox")
        '')
      ];
    };

  testScript =
    let
      api = "curl --fail -sk -u admin:${adminPassword} -H 'Content-Type: application/json' https://${host}/api/principal";
      principal = attrs: "${api} -d '${builtins.toJSON attrs}'";
    in
    ''
      mail.wait_for_unit("acme-${host}.service")
      mail.wait_for_unit("stalwart.service")
      mail.wait_for_unit("nginx.service")
      for port in [25, 443, 465, 587, 993]:
          mail.wait_for_open_port(port)
      mail.fail("ss -tln | grep -q ':143 '")

      # The loopback listener must not be reachable from outside.
      mail.succeed("ss -tln | grep -q '127.0.0.1:8080 '")

      mail.succeed("curl --fail -skL https://${host}/ -o /dev/null")
      mail.succeed("curl -s -o /dev/null -w '%{http_code}' http://${host}/ | grep -q 301")
      mail.succeed("curl -sk -D - https://${host}/ -o /dev/null | grep -qi strict-transport-security")

      mail.succeed("""${
        principal {
          type = "domain";
          name = domain;
        }
      }""")
      mail.succeed("""${
        principal {
          type = "individual";
          name = "alice";
          secrets = [ "alice-secret" ];
          emails = [ "alice@${domain}" ];
          roles = [ "user" ];
        }
      }""")
      mail.succeed("""${
        principal {
          type = "individual";
          name = "bob";
          secrets = [ "bob-secret" ];
          emails = [ "bob@${domain}" ];
          roles = [ "user" ];
        }
      }""")

      mail.succeed("mail-roundtrip")

      # STARTTLS on submission serves the shared ACME certificate.
      mail.succeed(
          "echo | openssl s_client -starttls smtp -connect ${host}:587 2>/dev/null"
          + " | openssl x509 -noout -fingerprint -sha256"
          + " | cut -d= -f2 > /tmp/smtp.fp"
      )
      mail.succeed(
          "openssl x509 -noout -fingerprint -sha256 -in /var/lib/acme/${host}/cert.pem"
          + " | cut -d= -f2 | diff - /tmp/smtp.fp"
      )
    '';
}
