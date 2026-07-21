"""The conformance report as a tree artifact.

A `genrule` cannot declare a directory output, and the page set is genuinely
dynamic — one page per module, so it changes whenever the registry does. That
makes `ctx.actions.declare_directory` the right primitive rather than a
workaround: the report is one logical artifact whose contents Bazel need not
enumerate ahead of time.

Gate results arrive as SPARQL JSON — unambiguous to parse, unlike TSV, whose
literal quoting and escaping fail quietly when mishandled.
"""

def _gate_report_impl(ctx):
    out = ctx.actions.declare_directory(ctx.label.name + "_src")

    args = ctx.actions.args()
    args.add("--out", out.path)
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

    # Each gate contributes both its query (the rationale that becomes the page
    # prose) and its results. Pairing them by index keeps the two lists in step;
    # a mismatch is a BUILD error rather than a silently mislabelled page.
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
        progress_message = "Rendering conformance report for %s" % ctx.label,
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
        "gate_ids": attr.string_list(
            mandatory = True,
            doc = "Gate ids, index-aligned with gate_queries and gate_results.",
        ),
        "gate_queries": attr.label_list(
            allow_files = [".rq"],
            mandatory = True,
            doc = "The .rq files. Their header comments become the page prose, " +
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
    doc = "Render gate results as an mdBook source tree for tbzl.dev.",
)
