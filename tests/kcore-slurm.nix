# Multi-node Slurm VM test: one controller, one login node, two compute nodes,
# one munge key, NFS /home, and an sbatch job that runs on both compute nodes.
# This is not part of the fast nix-checks job; run it on a machine that can
# build the full NixOS VMs.
{ pkgs }:
let
  mungeKey = "kcore-slurm-test-munge-key-0123456789abcdef";
  addresses = {
    control = "192.168.1.1";
    login = "192.168.1.2";
    compute1 = "192.168.1.3";
    compute2 = "192.168.1.4";
  };
  slurmConf = ''
    SelectType=select/cons_tres
    SelectTypeParameters=CR_Core_Memory
    ProctrackType=proctrack/cgroup
    TaskPlugin=task/cgroup,task/affinity
    SchedulerType=sched/backfill
    SrunPortRange=60001-63000
    NodeName=compute1 NodeAddr=${addresses.compute1} CoreSpecCount=1 MemSpecLimit=512 State=UNKNOWN
    NodeName=compute2 NodeAddr=${addresses.compute2} CoreSpecCount=1 MemSpecLimit=512 State=UNKNOWN
    PartitionName=batch Nodes=compute1,compute2 Default=YES MaxTime=INFINITE State=UP
  '';
  cgroupConf = ''
    CgroupPlugin=autodetect
    ConstrainCores=yes
    ConstrainRAMSpace=yes
    ConstrainDevices=yes
  '';
  base =
    name: roles:
    { ... }:
    {
      imports = [ ../modules/kcore-slurm.nix ];
      networking.hostName = name;
      networking.firewall.enable = false;
      networking.interfaces.eth1.ipv4.addresses = [
        {
          address = addresses.${name};
          prefixLength = 24;
        }
      ];
      systemd.tmpfiles.rules = [
        "f /etc/munge/munge.key 0400 munge munge - ${mungeKey}"
        "d /home 0755 root root -"
        "d /scratch 0755 root root -"
      ];
      kcore.slurm = {
        enable = true;
        nodeId = name;
        nodeAddress = addresses.${name};
        clusterName = "hpc-a";
        inherit roles;
        controlMachine = "control";
        controlAddr = addresses.control;
        iface = "eth1";
        memberAddresses = builtins.attrValues addresses;
        memberHosts = map (node: {
          name = node;
          address = addresses.${node};
        }) (builtins.attrNames addresses);
        inherit slurmConf cgroupConf;
        gresConf = "";
        users = [
          {
            name = "alice";
            uid = 2001;
            sshKeys = [ "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKcoreSlurmTestKey" ];
          }
        ];
        storage = {
          mode = "nfs";
          exports = [
            "/home"
            "/scratch"
          ];
        };
        gpu.driver = false;
        accounting.enabled = false;
      };
    };
in
pkgs.testers.runNixOSTest {
  name = "kcore-slurm";
  nodes = {
    control = base "control" [ "controller" ];
    login = base "login" [ "login" ];
    compute1 = base "compute1" [ "compute" ];
    compute2 = base "compute2" [ "compute" ];
  };
  testScript = ''
    start_all()
    control.wait_for_unit("slurmctld.service")
    control.wait_for_open_port(6817)
    for node in [compute1, compute2]:
        node.wait_for_unit("slurmd.service")
        node.wait_for_open_port(6818)
    login.wait_for_unit("multi-user.target")
    login.wait_until_succeeds("mountpoint -q /home")
    login.wait_until_succeeds("mountpoint -q /scratch")

    control.succeed("mkdir -p /home/alice && echo hello > /home/alice/hello && chown -R 2001:2001 /home/alice")
    login.succeed("test \"$(cat /home/alice/hello)\" = hello")

    login.succeed("su - alice -c 'sbatch --ntasks=2 --nodes=2 --wrap hostname --output=/home/alice/slurm-%j.out'")
    login.wait_until_succeeds("grep -q compute1 /home/alice/slurm-*.out && grep -q compute2 /home/alice/slurm-*.out", timeout=120)
  '';
}
