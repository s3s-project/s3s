// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use std::hint::black_box;

fn chunk_string_to_sign(fragments: &[&[u8]]) -> String {
    let date = s3s_sigv4::AmzDate::parse("20130524T000000Z").expect("valid timestamp");
    let seed = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
    s3s_sigv4::create_chunk_string_to_sign(&date, "us-east-1", "s3", seed, fragments)
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("control") => {}
        Some("sha256") => {
            black_box(s3s_sigv4::hex_sha256(b"canonical request", str::len));
        }
        Some("single-fragment") => {
            black_box(chunk_string_to_sign(&[b"hello world"]));
        }
        Some("fragments") => {
            black_box(chunk_string_to_sign(&[b"hello ", b"world"]));
        }
        Some("stream") => {
            let mut digest = s3s_sigv4::PayloadSha256::default();
            digest.update(b"signed body");
            black_box(digest.finalize());
        }
        Some("hmac") => {
            black_box(s3s_sigv4::calculate_signature_with_key("string to sign", &[0x31; 32]));
        }
        _ => panic!("unknown SigV4 probe"),
    }
}
