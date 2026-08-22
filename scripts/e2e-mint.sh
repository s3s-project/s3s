#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# SPDX-FileCopyrightText: 2023-2026 The s3s Authors

mkdir -p target
./scripts/s3s-proxy.sh > target/s3s-proxy.log &
sleep 3s
./scripts/mint.sh | tee target/mint.log
