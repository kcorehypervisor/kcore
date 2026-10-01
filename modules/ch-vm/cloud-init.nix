{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.ch-vm.vms;
  helpers = import ./helpers.nix { inherit lib; };
  inherit (helpers) generateMac vmInterfaces;

  netmaskToCidr =
    mask:
    {
      "255.0.0.0" = "8";
      "255.255.0.0" = "16";
      "255.255.128.0" = "17";
      "255.255.192.0" = "18";
      "255.255.224.0" = "19";
      "255.255.240.0" = "20";
      "255.255.248.0" = "21";
      "255.255.252.0" = "22";
      "255.255.254.0" = "23";
      "255.255.255.0" = "24";
      "255.255.255.128" = "25";
      "255.255.255.192" = "26";
      "255.255.255.224" = "27";
      "255.255.255.240" = "28";
      "255.255.255.248" = "29";
      "255.255.255.252" = "30";
    }
    .${mask} or (throw "unsupported netmask: ${mask}");

  ethernetsYaml =
    vmName: vmCfg:
    let
      one =
        iface:
        let
          head = "  vmnic${toString iface.index}:\n    match:\n      macaddress: \"${iface.macAddress}\"\n    set-name: eth${toString iface.index}\n";
          v6 = lib.optionalString (iface.ipv6 != null) "      - \"${iface.ipv6}/64\"\n";
          body =
            if iface.ipv4 == null then
              "    dhcp4: true\n"
              + lib.optionalString (iface.index != 0) "    dhcp4-overrides:\n      use-routes: false\n"
              + lib.optionalString (iface.ipv6 != null) ("    dhcp6: false\n    addresses:\n" + v6)
            else
              let
                net = cfg.networks.${iface.network};
                cidr = netmaskToCidr net.internalNetmask;
                route =
                  if iface.index == 0 then
                    "    gateway4: \"${net.gatewayIP}\"\n    nameservers:\n      addresses: [\"${net.gatewayIP}\"]\n"
                  else
                    "";
              in
              "    dhcp4: false\n    addresses:\n      - \"${iface.ipv4}/${cidr}\"\n" + v6 + route;
        in
        head + body;
    in
    "version: 2\nethernets:\n" + lib.concatMapStrings one (vmInterfaces vmName vmCfg);

  mkSeedIso =
    vmName: vmCfg:
    let
      userData =
        if vmCfg.cloudInitUserConfigFile != null then
          vmCfg.cloudInitUserConfigFile
        else
          pkgs.writeText "${vmName}-user-data" ''
            #cloud-config
            hostname: ${vmName}
            users:
              - default
              - name: kcore
                gecos: kcore default user
                groups: [sudo]
                shell: /bin/bash
                lock_passwd: false
            ssh_pwauth: true
            chpasswd:
              expire: false
              users:
                - name: kcore
                  password: kcore
          '';
      networkConfig =
        if vmCfg.cloudInitNetworkConfigFile != null then
          vmCfg.cloudInitNetworkConfigFile
        else if vmCfg.extraNics != [ ] then
          pkgs.writeText "${vmName}-network-config" (ethernetsYaml vmName vmCfg)
        else
          pkgs.writeText "${vmName}-network-config" (
            ''
              version: 2
              ethernets:
                vmnic0:
                  match:
                    macaddress: "${generateMac vmName}"
                  set-name: eth0
                  dhcp4: true
            ''
            + lib.optionalString (
              vmCfg.dhcpReservedIPv6 != null
            ) "    dhcp6: false\n    addresses:\n      - \"${vmCfg.dhcpReservedIPv6}/64\"\n"
          );

      instanceId = if vmCfg.cloudInitInstanceId != null then vmCfg.cloudInitInstanceId else vmName;
      metaData = pkgs.writeText "${vmName}-meta-data" ''
        instance-id: ${instanceId}
        local-hostname: ${vmName}
      '';
    in
    pkgs.runCommand "kcore-seed-${vmName}.iso"
      {
        nativeBuildInputs = [ pkgs.cloud-utils ];
      }
      ''
        cloud-localds \
          --network-config ${networkConfig} \
          "$out" ${userData} ${metaData}
      '';
in
{
  config = lib.mkIf cfg.enable {
    systemd.tmpfiles.rules = [
      "d ${cfg.socketDir} 0755 root root -"
      "d /var/lib/kcore/seeds 0755 root root -"
    ];

    environment.etc = lib.mapAttrs' (
      vmName: vmCfg:
      lib.nameValuePair "kcore/seeds/${vmName}.iso" {
        source = mkSeedIso vmName vmCfg;
      }
    ) cfg.virtualMachines;
  };
}
