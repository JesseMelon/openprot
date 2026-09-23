# Licensed under the Apache-2.0 license
# SPDX-License-Identifier: Apache-2.0

"""Rules for recording and checking the hardware each process takes.

Pigweed's `rust_app` builds the app's linker script from a fixed list of
annotation sections and does not let a caller extend it. `rust_app` here is that
macro with one attribute threaded through, so apps also get
`.pw_kernel.annotations.hw_claim`. `hw_claim_test` reads that section back out
of the assembled image.
"""

load("@pigweed//pw_kernel/tooling:app_linker_script.bzl", "app_linker_script")
load("@pigweed//pw_kernel/tooling:rust_app.bzl", "rust_app_codegen")
load("@pigweed//pw_kernel/tooling:system_image.bzl", "SystemImageInfo")
load("@rules_rust//rust:defs.bzl", "rust_binary")

def rust_app(name, codegen_crate_name, srcs, deps = None, system_config = None, **kwargs):
    """Builds a userspace app whose linker script carries the hw_claim section.

    Args:
        name: The name of the target.
        codegen_crate_name: Name to use for the generated codegen crate.
        srcs: The list of source files for the app.
        deps: The list of dependencies for the app.
        system_config: System config file which defines the system.
        **kwargs: Other attributes passed to the underlying rules.
    """
    if deps == None:
        deps = []

    rust_app_codegen(
        name = codegen_crate_name,
        app_name = name,
        system_config = system_config,
        **kwargs
    )

    linker_script_name = name + ".linker_script"

    app_linker_script(
        name = linker_script_name,
        app_name = name,
        pigweed_sections = "//util/hw_claim:linker_sections.ld.jinja",
        system_config = system_config,
        tags = kwargs.get("tags", []),
    )

    rust_binary(
        name = name,
        srcs = srcs,
        deps = deps + [
            ":" + codegen_crate_name,
            ":" + linker_script_name,
        ],
        **kwargs
    )

def _hw_claim_test_impl(ctx):
    elf = ctx.attr.image[SystemImageInfo].elf
    launcher = ctx.actions.declare_file(ctx.attr.name + ".sh")

    ctx.actions.write(
        output = launcher,
        content = "exec ./{checker} ./{elf} {flags}\n".format(
            checker = ctx.executable._checker.short_path,
            elf = elf.short_path,
            flags = "--expect-conflict" if ctx.attr.expect_conflict else "",
        ),
        is_executable = True,
    )

    return [DefaultInfo(
        executable = launcher,
        runfiles = ctx.runfiles(files = [elf, ctx.executable._checker]),
    )]

hw_claim_test = rule(
    implementation = _hw_claim_test_impl,
    test = True,
    attrs = {
        "expect_conflict": attr.bool(
            doc = "Invert the check: pass only if the image does conflict.",
            default = False,
        ),
        "image": attr.label(
            doc = "Assembled system image to check.",
            mandatory = True,
            providers = [SystemImageInfo],
        ),
        "_checker": attr.label(
            executable = True,
            cfg = "exec",
            default = "//util/hw_claim:check",
        ),
    },
    doc = "Fails if two processes in the image take the same hardware.",
)
