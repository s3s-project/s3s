#!/usr/bin/env python
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 Nugine

from pathlib import Path


def main():
    crates = Path("crates")
    for crate in crates.iterdir():
        license_file = crate / "LICENSE"
        if not license_file.exists():
            license_file.symlink_to("../../LICENSE")


if __name__ == "__main__":
    main()
