# Host metrics: node_exporter on every node; optional VictoriaMetrics on controllers.
#
# Compatible with NixOS 25.05+: avoids `services.victoriametrics.checkConfig`
# (newer only) and avoids baking promscrape config into the Nix eval (25.05
# always dry-runs that at build time, which fails for runtime file_sd paths).
{
  config,
  lib,
  ...
}:
let
  cfg = config.kcore.metrics;
  port = toString cfg.listenPort;
  scrapeSources = lib.unique (builtins.filter (s: s != "") cfg.allowedScrapeSources);
  scrapeConfigPath = "/etc/kcore/metrics/scrape.yml";
in
{
  options.kcore.metrics = {
    enable = lib.mkEnableOption "Prometheus node_exporter for host metrics";

    listenPort = lib.mkOption {
      type = lib.types.port;
      default = 9100;
      description = "TCP port for node_exporter.";
    };

    allowedScrapeSources = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [
        "192.168.40.10"
        "192.168.40.0/24"
      ];
      description = ''
        IPv4 addresses or CIDRs allowed to scrape node_exporter. The port is
        never added to the world-open allowedTCPPorts list.
      '';
    };

    server = {
      enable = lib.mkEnableOption "VictoriaMetrics scrape/store on this node (controllers)";

      listenAddress = lib.mkOption {
        type = lib.types.str;
        default = ":8428";
        description = "HTTP listen address for VictoriaMetrics (PromQL / vmui).";
      };

      retentionPeriod = lib.mkOption {
        type = lib.types.str;
        default = "15d";
        description = "How long VictoriaMetrics retains samples.";
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Open the VictoriaMetrics HTTP port in the host firewall.";
      };

      fileSdDir = lib.mkOption {
        type = lib.types.str;
        default = "/etc/kcore/metrics/file_sd";
        description = ''
          Directory of Prometheus file_sd JSON files. The controller rewrites
          `nodes.json` here from the node inventory.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        # Keep the classic iptables backend used by existing kcore installs.
        # NixOS 25.05 may enable nftables by default; that rejects extraCommands.
        networking.nftables.enable = lib.mkForce false;

        services.prometheus.exporters.node = {
          enable = true;
          port = cfg.listenPort;
          # Restricted source rules below; do not open 9100 to the world.
          openFirewall = false;
        };

        # iptables path (default when networking.nftables.enable is false).
        # Use iptables (not ip46tables): scrape sources are IPv4 addresses/CIDRs.
        networking.firewall.extraCommands = lib.mkIf (!config.networking.nftables.enable) (
          lib.mkAfter (
            lib.concatMapStrings (src: ''
              iptables -A nixos-fw -p tcp --dport ${port} -s ${lib.escapeShellArg src} -j nixos-fw-accept
            '') scrapeSources
          )
        );
        networking.firewall.extraStopCommands = lib.mkIf (!config.networking.nftables.enable) (
          lib.mkAfter (
            lib.concatMapStrings (src: ''
              iptables -D nixos-fw -p tcp --dport ${port} -s ${lib.escapeShellArg src} -j nixos-fw-accept || true
            '') scrapeSources
          )
        );

        # nftables path when operators enable networking.nftables.enable.
        networking.firewall.extraInputRules = lib.mkIf config.networking.nftables.enable (
          lib.concatMapStrings (src: ''
            ip saddr ${src} tcp dport ${port} accept comment "kcore-node-exporter"
          '') scrapeSources
        );
      }

      (lib.mkIf cfg.server.enable {
        environment.etc."kcore/metrics/scrape.yml".text = ''
          scrape_configs:
            - job_name: node
              file_sd_configs:
                - files:
                    - ${cfg.server.fileSdDir}/*.json
        '';

        systemd.tmpfiles.rules = [
          "d /etc/kcore/metrics 0755 root root -"
          "d ${cfg.server.fileSdDir} 0755 root root -"
          "f ${cfg.server.fileSdDir}/nodes.json 0644 root root - []"
        ];

        services.victoriametrics = {
          enable = true;
          listenAddress = cfg.server.listenAddress;
          retentionPeriod = cfg.server.retentionPeriod;
          # Keep prometheusConfig empty so older nixpkgs does not dry-run a
          # missing runtime file_sd path at build time.
          prometheusConfig = { };
          extraOptions = [ "-promscrape.config=${scrapeConfigPath}" ];
        };

        networking.firewall.allowedTCPPorts = lib.optionals cfg.server.openFirewall [ 8428 ];
      })
    ]
  );
}
