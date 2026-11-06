# Mail server: Stalwart (SMTP, IMAP, JMAP, ManageSieve, CalDAV/CardDAV) from
# the pinned nixpkgs, with nginx terminating HTTPS for the web admin and JMAP.
#
# Nix owns the listeners, TLS, storage and the fallback admin. Domains,
# accounts and DKIM keys live in Stalwart's internal directory and are managed
# in the web admin. Every key set here is declared local, so a web admin edit
# cannot shadow it from the database.
{
  config,
  lib,
  ...
}:
let
  cfg = config.kcore.mail;
  stalwart = config.services.stalwart;
  cert = config.security.acme.certs.${cfg.hostname};
  publicListener = port: extra: { bind = [ "[::]:${toString port}" ]; } // extra;

  # Stalwart's built-in local keys (crates/common/src/manager/config.rs).
  # Setting config.local-keys replaces them, so they are repeated here.
  defaultLocalKeys = [
    "store.*"
    "directory.*"
    "tracer.*"
    "!server.blocked-ip.*"
    "!server.allowed-ip.*"
    "server.*"
    "certificate.*"
    "config.local-keys.*"
    "authentication.fallback-admin.*"
    "cluster.*"
    "storage.data"
    "storage.blob"
    "storage.lookup"
    "storage.fts"
    "storage.directory"
    "enterprise.license-key"
  ];
in
{
  options.kcore.mail = {
    enable = lib.mkEnableOption "the Stalwart mail server";

    hostname = lib.mkOption {
      type = lib.types.str;
      example = "mail.example.com";
      description = ''
        Public name of the server: the SMTP banner, the TLS certificate and the
        web admin URL. Its PTR record must point back at this name.
      '';
    };

    extraHostnames = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "autoconfig.example.com"
        "mta-sts.example.com"
      ];
      description = ''
        Further names served by the same nginx virtual host and certificate,
        such as autoconfig and MTA-STS hosts. Each needs a DNS record before
        the certificate can be issued.
      '';
    };

    acme = {
      email = lib.mkOption {
        type = lib.types.str;
        example = "postmaster@example.com";
        description = "Contact address for the Let's Encrypt account.";
      };

      staging = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Use the Let's Encrypt staging directory. Turn this on for first
          deployments to stay clear of the production rate limits.
        '';
      };
    };

    adminSecretFile = lib.mkOption {
      type = lib.types.str;
      example = "/run/agenix/stalwart-admin";
      description = ''
        File holding the password, or a SHA-512 crypt / bcrypt / Argon2 hash
        of it, for the fallback `admin` account of the web admin. It is passed
        as a systemd credential and must live outside the Nix store.
      '';
    };

    httpPort = lib.mkOption {
      type = lib.types.port;
      default = 8080;
      description = "Loopback port of Stalwart's HTTP listener; nginx proxies to it.";
    };

    manageSieve.enable = lib.mkEnableOption "the ManageSieve listener on port 4190";
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !lib.hasPrefix builtins.storeDir cfg.adminSecretFile;
        message = "kcore.mail.adminSecretFile must not be in the Nix store.";
      }
    ];

    services.stalwart = {
      enable = true;
      credentials.admin_secret = cfg.adminSecretFile;
      settings = {
        server = {
          inherit (cfg) hostname;
          tls = {
            enable = true;
            implicit = false;
          };
          listener = {
            smtp = publicListener 25 { protocol = "smtp"; };
            submission = publicListener 587 { protocol = "smtp"; };
            submissions = publicListener 465 {
              protocol = "smtp";
              tls.implicit = true;
            };
            imaptls = publicListener 993 {
              protocol = "imap";
              tls.implicit = true;
            };
            http = {
              bind = [ "127.0.0.1:${toString cfg.httpPort}" ];
              protocol = "http";
            };
          }
          // lib.optionalAttrs cfg.manageSieve.enable {
            sieve = publicListener 4190 { protocol = "managesieve"; };
          };
        };

        # nginx owns the certificate; Stalwart reads the same files.
        certificate.default = {
          cert = "%{file:${cert.directory}/fullchain.pem}%";
          private-key = "%{file:${cert.directory}/key.pem}%";
          default = true;
        };

        # Behind nginx, so links in JMAP sessions and the web admin must name
        # the public URL, not the loopback listener.
        http = {
          url = "'https://${cfg.hostname}'";
          use-x-forwarded = true;
        };

        authentication.fallback-admin = {
          user = "admin";
          secret = "%{file:/run/credentials/stalwart.service/admin_secret}%";
        };

        config.local-keys = defaultLocalKeys ++ [ "http.*" ];
      };
    };

    # acme-<host> writes a self-signed placeholder before the real order, so
    # Stalwart always has a certificate file to read at start.
    systemd.services.stalwart = {
      wants = [ "acme-${cfg.hostname}.service" ];
      after = [ "acme-${cfg.hostname}.service" ];
    };

    users.users.${stalwart.user}.extraGroups = [ cert.group ];

    security.acme = {
      acceptTerms = true;
      defaults = {
        inherit (cfg.acme) email;
        server = lib.mkIf cfg.acme.staging "https://acme-staging-v02.api.letsencrypt.org/directory";
      };
      # Stalwart has no reload; a renewal restarts it.
      certs.${cfg.hostname}.reloadServices = [ "stalwart.service" ];
    };

    services.nginx = {
      enable = true;
      recommendedTlsSettings = true;
      recommendedOptimisation = true;
      recommendedGzipSettings = true;
      recommendedProxySettings = true;
      virtualHosts.${cfg.hostname} = {
        serverAliases = cfg.extraHostnames;
        forceSSL = true;
        enableACME = true;
        extraConfig = ''
          add_header Strict-Transport-Security "max-age=63072000" always;
          client_max_body_size 64m;
        '';
        locations."/" = {
          proxyPass = "http://127.0.0.1:${toString cfg.httpPort}";
          proxyWebsockets = true;
        };
      };
    };

    networking.firewall.allowedTCPPorts = [
      25
      80
      443
      465
      587
      993
    ]
    ++ lib.optional cfg.manageSieve.enable 4190;
  };
}
