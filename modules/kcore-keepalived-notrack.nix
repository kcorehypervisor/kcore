# Protocol 112 must skip conntrack before keepalived transmits. The VM
# masquerade otherwise rewrites the advert source, and a second master
# keeps the VIP. Shared by every module that runs a VRRP instance.
{ pkgs }:
pkgs.writeShellScript "kcore-keepalived-notrack" ''
  ${pkgs.iptables}/bin/iptables -C nixos-fw -p 112 -j nixos-fw-accept 2>/dev/null || ${pkgs.iptables}/bin/iptables -A nixos-fw -p 112 -j nixos-fw-accept || true
  ${pkgs.iptables}/bin/iptables -t raw -C PREROUTING -p 112 -j CT --notrack 2>/dev/null || ${pkgs.iptables}/bin/iptables -t raw -I PREROUTING 1 -p 112 -j CT --notrack
  ${pkgs.iptables}/bin/iptables -t raw -C OUTPUT -p 112 -j CT --notrack 2>/dev/null || ${pkgs.iptables}/bin/iptables -t raw -I OUTPUT 1 -p 112 -j CT --notrack
''
