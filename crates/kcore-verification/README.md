# kcore-verification

Design model for path checks, Nix escaping, security groups, replication reservations, compensation, VM placement, and disk-layout admission.

These functions are not the ones in `kcore-sanitize`, `kcore-disko-types`, or the controller. A passing test or proof here does not prove the matching product function.

`cargo test -p kcore-verification` runs the unit tests and the proptest suites.

`src/proofs.rs` is compiled only by `cargo kani -p kcore-verification`. The harnesses are:

- `segment_acceptance_implies_safe`
- `port_range_accepts_ordered_bounds`
- `comment_strip_keeps_byte_length`
- `dangerous_role_is_never_safe`

Those four are not part of the GitHub `kani` job. That job stays on the three path scanners in `kcore-sanitize`.

The TLA+ models for this crate live in `specs/tla/model/` and are checked by `make test-tla` after the product specs in `specs/tla/`.
