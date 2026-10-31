# NVIDIA vGPU host

kcore stages the NVIDIA Virtual GPU Manager and loads it from the node's Nix
configuration. The proprietary `.run` file stays out of git. The node downloads
it, checks the SHA256, and adds it to the Nix store.

The node has to be running a kcore generation that reports `vgpuManagerRun`.
Roll that generation out with `kctl update cluster` before enabling the host.

```bash
kctl node vgpu enable node-193 \
  --host-card 0000:61:00.0 \
  --url http://192.168.40.10:18083/virtual-gpu-manager-8_v8.2/NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run \
  --sha256 2bc7f9be263d45a92331d781df33c14af1af01fcfa45e80102e80caf1ffaa74d \
  --file NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run
```

When the card is still on `nouveau` or has no IOMMU group, the command
schedules a reboot. `kctl node vgpu enable --no-reboot` leaves that reboot to
`kctl node reboot node-193`. The reboot is refused while a VM is set to run
unless `--force` is set.

Create a guest with one slice:

```bash
kctl create vm vgpu-01 --target-node node-193 --vgpu A2-4C ...
kctl update vm vgpu-01 --vgpu A2-4C
```

`kctl update vm --vgpu` and `--gpu` apply only while the VM is stopped. An
empty target node is allowed for a vGPU request: the controller places the VM
on a ready node that can supply the profile.

The same operations are `ConfigureNodeVgpu`, `GetNodeVgpu`, and `RebootNode`
on the controller, `StageVgpuManager` on the node, `kind: NodeVgpu` for
`kctl apply -f`, the `node-vgpu` MCP kind, and Terraform `kcore_vgpu_host` plus
`kcore_vm.vgpus`. See `docs/gpu-passthrough.md`.
