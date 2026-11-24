{
  kcore.kubernetes = {
    enable = true;
    role = "worker";
    advertiseAddress = "10.0.0.2";
    apiServerEndpoint = "10.0.0.1:6443";
  };
}
