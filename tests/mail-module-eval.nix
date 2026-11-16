# Eval-only guard: kcore.mail evaluates with and without ManageSieve, renders
# a Stalwart configuration behind nginx, and leaves a host untouched when
# disabled.
{
  pkgs,
  lib,
  nixosSystem,
}:
let
  base = {
    networking.hostName = "mail-eval";
    system.stateVersion = "25.11";
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "nodev";
      fsType = "tmpfs";
    };
  };

  mkHost =
    mail:
    nixosSystem {
      inherit (pkgs) system;
      modules = [
        ../modules/kcore-mail.nix
        (_: base // { kcore.mail = mail; })
      ];
    };

  enabled = mkHost {
    enable = true;
    hostname = "mail.example.com";
    extraHostnames = [ "autoconfig.example.com" ];
    acme = {
      email = "postmaster@example.com";
      staging = true;
    };
    adminSecretFile = "/run/agenix/stalwart-admin";
  };

  sieve = mkHost {
    enable = true;
    hostname = "mail.example.com";
    acme.email = "postmaster@example.com";
    adminSecretFile = "/run/agenix/stalwart-admin";
    manageSieve.enable = true;
  };

  disabled = mkHost { enable = false; };

  cfg = enabled.config;
  settings = cfg.services.stalwart.settings;
  listeners = settings.server.listener;
  vhost = cfg.services.nginx.virtualHosts."mail.example.com";
  ports = cfg.networking.firewall.allowedTCPPorts;
in
assert cfg.services.stalwart.enable;
assert settings.server.hostname == "mail.example.com";
assert listeners.http.bind == [ "127.0.0.1:8080" ];
assert listeners.imaptls.tls.implicit;
assert !(listeners ? imap);
assert !(listeners ? sieve);
assert settings.http.url == "'https://mail.example.com'";
assert lib.elem "http.*" settings.config.local-keys;
assert lib.elem "server.*" settings.config.local-keys;
assert lib.hasInfix "/var/lib/acme/mail.example.com/fullchain.pem"
  settings.certificate.default.cert;
assert cfg.services.stalwart.credentials.admin_secret == "/run/agenix/stalwart-admin";
assert vhost.enableACME && vhost.forceSSL;
assert vhost.serverAliases == [ "autoconfig.example.com" ];
assert vhost.locations."/".proxyPass == "http://127.0.0.1:8080";
assert lib.hasInfix "staging" cfg.security.acme.defaults.server;
assert lib.elem "acme-mail.example.com.service" cfg.systemd.services.stalwart.after;
assert lib.elem "nginx" cfg.users.users.${cfg.services.stalwart.user}.extraGroups;
assert lib.all (p: lib.elem p ports) [
  25
  80
  443
  465
  587
  993
];
assert !(lib.elem 4190 ports);
assert !(lib.elem 8080 ports);
assert sieve.config.services.stalwart.settings.server.listener ? sieve;
assert lib.elem 4190 sieve.config.networking.firewall.allowedTCPPorts;
assert !(lib.hasInfix "staging" sieve.config.security.acme.defaults.server);
assert disabled.config.services.stalwart.enable == false;
assert disabled.config.services.nginx.enable == false;
# Forces the rendered stalwart.toml and the whole system to evaluate.
assert lib.isString cfg.system.build.toplevel.drvPath;
pkgs.runCommand "mail-module-eval-ok" { } ''
  echo ok > "$out"
''
