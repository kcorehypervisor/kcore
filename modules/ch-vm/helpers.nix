{ lib }:
rec {
  tapName = vmName: "tap-${builtins.substring 0 8 (builtins.hashString "sha256" vmName)}";

  generateMac =
    vmName:
    let
      hash = builtins.hashString "sha256" vmName;
      hexChars = lib.stringToCharacters hash;
      byte = n: lib.concatStrings (lib.sublist (n * 2) 2 hexChars);
    in
    "52:54:00:${byte 0}:${byte 1}:${byte 2}";

  # NIC 0 keeps the historical tap and MAC (hash of the VM name alone) so an
  # existing single-NIC guest does not move. Later NICs salt the hash with
  # the index.
  vmInterfaces =
    vmName: vmCfg:
    let
      primary = {
        index = 0;
        network = vmCfg.network;
        macAddress = if vmCfg.macAddress != null then vmCfg.macAddress else generateMac vmName;
        ipv4 = vmCfg.dhcpReservedIPv4;
        ipv6 = vmCfg.dhcpReservedIPv6;
        tap = tapName vmName;
        unit = "kcore-tap-${vmName}";
      };
      extras = lib.imap0 (
        i: nic:
        let
          index = i + 1;
          salt = "${vmName}#${toString index}";
        in
        {
          index = index;
          network = nic.network;
          macAddress = if nic.macAddress != null then nic.macAddress else generateMac salt;
          ipv4 = nic.ipv4;
          ipv6 = nic.ipv6;
          tap = tapName salt;
          unit = "kcore-tap-${vmName}-${toString index}";
        }
      ) vmCfg.extraNics;
    in
    [ primary ] ++ extras;
}
