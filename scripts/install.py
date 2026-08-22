#!/usr/bin/env python
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

import argparse
import subprocess
from dataclasses import dataclass

INSTALLERS = {}


def installer(name):
    def decorator(f):
        INSTALLERS[name] = f
        return f

    return decorator


@dataclass(kw_only=True)
class CliArgs:
    name: str

    offline: bool = False

    @staticmethod
    def parse():
        parser = argparse.ArgumentParser()
        parser.add_argument("name", type=str, choices=list(INSTALLERS.keys()))
        parser.add_argument("--offline", action="store_true")
        args = parser.parse_args()
        return CliArgs(**args.__dict__)


def sh(cmd):
    print(cmd)
    subprocess.run(cmd, shell=True, check=True, stdin=subprocess.DEVNULL)


def cargo_install(
    args: CliArgs,
    package: str,
    *,
    features: list[str] | None = None,
    bin: str | None = None,
    force: bool = True,
):
    opt_offline = "--offline" if args.offline else ""
    opt_features = f"--features {','.join(features)}" if features else ""
    opt_bin = f"--bin {bin}" if bin else ""
    opt_force = "--force" if force else ""
    sh(
        f"cargo install --path crates/{package} --locked "
        f"{opt_offline} {opt_features} {opt_bin} {opt_force}"
    )


@installer("s3s-fs")
def install_s3s_fs(args: CliArgs):
    cargo_install(args, "s3s-fs", features=["binary"])


@installer("s3s-proxy")
def install_s3s_proxy(args: CliArgs):
    cargo_install(args, "s3s-proxy")


@installer("s3s-e2e")
def install_s3s_e2e(args: CliArgs):
    sh("touch crates/s3s-e2e/build.rs")
    cargo_install(args, "s3s-e2e")


@installer("all")
def install_all(args: CliArgs):
    if not args.offline:
        sh("cargo fetch")
        args.offline = True

    for name, f in INSTALLERS.items():
        if name != "all":
            f(args)


def main(args: CliArgs):
    INSTALLERS[args.name](args)


if __name__ == "__main__":
    main(CliArgs.parse())
