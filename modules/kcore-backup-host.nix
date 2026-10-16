# Installed backup appliance. This is a NixOS host for a repository disk and,
# later, the kcore-vault module. It does not import the hypervisor, Ceph,
# Kubernetes, or the node agent. The local screen is the kcore console in
# vault mode; the flake module passes the console package.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.kcoreBackup;
in
{
  options.services.kcoreBackup = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Apply the backup appliance profile.";
    };

    version = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Product version written to /etc/kcore/version.";
    };
  };

  config = lib.mkIf cfg.enable {
    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    # A later nixpkgs default of broker makes nixos-rebuild refuse to switch
    # a running host. Callers do not set this.
    services.dbus.implementation = "dbus";

    boot.loader.systemd-boot.enable = true;
    boot.loader.systemd-boot.configurationLimit = 8;
    boot.loader.timeout = 5;
    boot.loader.efi.canTouchEfiVariables = true;
    boot.kernel.sysctl."kernel.panic" = 10;

    networking.firewall.enable = lib.mkDefault true;
    networking.firewall.allowedTCPPorts = [
      22
      9191
    ];

    services.openssh.enable = lib.mkDefault true;
    services.openssh.settings.PermitRootLogin = lib.mkDefault "yes";
    services.openssh.settings.PasswordAuthentication = lib.mkDefault true;

    environment.etc."os-release".text = ''
      NAME="kcoreOS"
      PRETTY_NAME="kcoreOS backup"
      ID=nixos
      VERSION_ID="25.05"
      VERSION="25.05 (kcoreOS backup)"
      VERSION_CODENAME=kcoreos
      HOME_URL="https://github.com/kcorehypervisor/kcore"
      SUPPORT_URL="https://github.com/kcorehypervisor/kcore"
      BUG_REPORT_URL="https://github.com/kcorehypervisor/kcore/issues"
    '';

    environment.etc."kcore/version" = lib.mkIf (cfg.version != "") {
      text = "${cfg.version}\n";
      mode = "0644";
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/kcore-vault 0750 root root -"
      "d /etc/kcore 0755 root root -"
      "d /etc/kcore/recovery 0700 root root -"
    ];

    environment.systemPackages = with pkgs; [
      cryptsetup
      iproute2
      jq
    ];

    services.kcore-console = {
      enable = true;
      product = "vault";
    };
  };
}
