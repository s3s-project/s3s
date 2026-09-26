// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Integration tests for the HTTP/3 transport, compiled as a single test target so that the
//! shared helpers in common.rs are linked once.

mod common;
mod generic_service;
mod s3_api;
