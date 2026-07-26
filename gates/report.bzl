"""The conformance report as a `tomato.gate.v1` wire artifact.

This used to declare a DIRECTORY: the report was an mdBook source tree, one page
per module, so the file set changed whenever the registry did and
`ctx.actions.declare_directory` was the right primitive. The rendering has since
moved to tomato-bazel/docs (docs.tbzl.dev/conformance/), which means the output
is now a single message and a plain declared file will do.

Gate results arrive as SPARQL JSON — unambiguous to parse, unlike TSV, whose
literal quoting and escaping fail quietly when mishandled.

`generated_at` comes in as a build setting rather than a clock read, so the
report stays a pure function of (projection x queries) and a rebuild with no
input change produces identical bytes.
"""

load("@bazel_skylib//rules:common_settings.bzl", "BuildSettingInfo")

def _gate_report_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".binpb")

    args = ctx.actions.args()
    args.add("--out-binpb", out.path)
    args.add("--generated-at", ctx.attr.generated_at[BuildSettingInfo].value)
    args.add("--generated-from", ctx.attr.generated_from[BuildSettingInfo].value)
    args.add("--bom", ctx.file.bom)
    args.add("--inventory", ctx.file.inventory)
    args.add("--selections", ctx.file.selections)
    args.add("--upgrades", ctx.file.upgrades)
    args.add("--declared", ctx.file.declared)

    inputs = [
        ctx.file.bom,
        ctx.file.inventory,
        ctx.file.selections,
        ctx.file.upgrades,
        ctx.file.declared,
    ]

    # Each gate contributes both its query (the rationale that becomes the docs
    # prose) and its results. Pairing them by index keeps the two lists in step;
    # a mismatch is a BUILD error rather than a silently mislabelled gate.
    if len(ctx.attr.gate_ids) != len(ctx.files.gate_queries):
        fail("gate_ids and gate_queries must be the same length")
    if len(ctx.attr.gate_ids) != len(ctx.files.gate_results):
        fail("gate_ids and gate_results must be the same length")

    for i, gate_id in enumerate(ctx.attr.gate_ids):
        query = ctx.files.gate_queries[i]
        result = ctx.files.gate_results[i]
        args.add("--gate", "{}={}={}".format(gate_id, query.path, result.path))
        inputs.append(query)
        inputs.append(result)

    ctx.actions.run(
        executable = ctx.executable._tool,
        arguments = [args],
        inputs = inputs,
        outputs = [out],
        mnemonic = "GateReport",
        progress_message = "Projecting conformance report for %s" % ctx.label,
    )

    return [DefaultInfo(files = depset([out]))]

gate_report = rule(
    implementation = _gate_report_impl,
    attrs = {
        "bom": attr.label(
            allow_single_file = True,
            mandatory = True,
            doc = "rules_tomato//bom:versions.json — the intended version per atom.",
        ),
        "inventory": attr.label(
            allow_single_file = True,
            mandatory = True,
            doc = "SPARQL JSON results for the per-module inventory query.",
        ),
        "selections": attr.label(
            allow_single_file = True,
            mandatory = True,
            doc = "SPARQL JSON: which module resolves which version of which atom.",
        ),
        "upgrades": attr.label(
            allow_single_file = True,
            mandatory = True,
            doc = "SPARQL JSON: modules silently upgraded past their declared pin.",
        ),
        "declared": attr.label(
            allow_single_file = True,
            mandatory = True,
            doc = "SPARQL JSON: atoms our modules DECLARE, as opposed to reach " +
                  "transitively. Only these can carry a BOM pin.",
        ),
        "generated_at": attr.label(
            mandatory = True,
            providers = [BuildSettingInfo],
            doc = "A string_flag holding an RFC 3339 timestamp. A FLAG, not a " +
                  "clock read: reading build time would break reproducibility, " +
                  "and defaulting to 'now' downstream would make a stale " +
                  "report look fresh, which is the failure this field exists " +
                  "to expose. CI sets it; local builds leave it empty.",
        ),
        "generated_from": attr.label(
            mandatory = True,
            providers = [BuildSettingInfo],
            doc = "A string_flag naming where the projection came from — the " +
                  "bazel-registry URL, ideally with the commit it was cloned at.",
        ),
        "gate_ids": attr.string_list(
            mandatory = True,
            doc = "Gate ids, index-aligned with gate_queries and gate_results.",
        ),
        "gate_queries": attr.label_list(
            allow_files = [".rq"],
            mandatory = True,
            doc = "The .rq files. Their header comments become the rationale, " +
                  "so documentation cannot drift from the gate it documents.",
        ),
        "gate_results": attr.label_list(
            allow_files = [".json"],
            mandatory = True,
            doc = "SPARQL JSON results, index-aligned with gate_queries.",
        ),
        "_tool": attr.label(
            default = "//crates/gate-report:gate-report",
            executable = True,
            cfg = "exec",
        ),
    },
    doc = "Project gate results into a tomato.gate.v1 ConformanceReport.",
)
