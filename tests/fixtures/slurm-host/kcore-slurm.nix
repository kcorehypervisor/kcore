{ ... }:
{
  kcore.slurm = {
    enable = true;
    nodeId = "node-2";
    nodeAddress = "10.0.0.2";
    clusterName = "hpc-a";
    roles = [ "compute" ];
    controlMachine = "node-1";
    controlAddr = "10.0.0.1";
    memberAddresses = [
      "10.0.0.1"
      "10.0.0.2"
    ];
    slurmConf = ''
      NodeName=node-2 NodeAddr=10.0.0.2 State=UNKNOWN
      PartitionName=batch Nodes=node-2 Default=YES MaxTime=INFINITE State=UP
    '';
  };
}
