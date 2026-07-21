# Pinned fixtures

Real `MODULE.bazel` files vendored from the registry, used by
`tests/fixtures.rs` via `include_str!`.

**These are evidence, not source.** Each one is here because it exhibits a
shape a gate must catch or must not flag — `d3_live_violation` is `vpn`'s
unnamespaced `oci.pull` on the shared `@rules_oci` extension, `d3_after` is the
fix `rules_k8s` shipped after that bug broke its first consumer. They are
**not** refreshed when the upstream module is remediated; the point is that the
detector keeps working against the shape that once existed.

The `.fixture` suffix exists so `buildifier` does not treat them as Bazel files.
It would reformat them — inserting blank lines between `bazel_dep` groups — and
a reformatted fixture is no longer a faithful copy of what was published.
