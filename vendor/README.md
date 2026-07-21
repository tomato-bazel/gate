# Vendored upstream registry snapshot

Byte-exact copies of `MODULE.bazel` from upstream (the BCR), laid out in
registry format so `FsRegistry` reads them like any other registry.

**Read-only, in both directions.** `gate-snapshot` fetches from upstream and
writes here; nothing in this repository publishes anywhere. Getting a module
into the BCR means opening a pull request against
`bazelbuild/bazel-central-registry`, which this tooling has no path to do.

## Why vendored rather than fetched

The projection is deliberately filesystem-only, so a certificate is a function
of the commit rather than of network state on the day it ran. The network lives
in `gate-snapshot`, which runs occasionally and whose output is reviewed here.

At 192 module versions and ~1.2 MB, vendoring stops being a cost and becomes
the point: the third-party trusted computing base is **enumerable**, and
growing it shows up as added files in a pull request instead of as a number in
a report somebody has to notice changed. That is a stronger property than the
C4 gate originally specified.

## Do not reformat

`buildifier` is scoped away from this directory in CI. These files must match
upstream byte for byte or `snapshot --check` — which compares the vendored
bytes against the live registry — reports spurious drift forever.

## Refreshing

    bazel run //crates/gate-snapshot:snapshot -- \
      --registry <path-to-bazel-registry> --out vendor/bcr

    # in CI, to detect staleness without writing:
    ... --check
