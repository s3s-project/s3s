// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Checks the committed seed corpus of the `multipart_parser` fuzz target.
//!
//! A seed is only worth committing while it still exercises what it was added
//! for, and a corpus nobody re-reads rots silently when the parser changes. So
//! every seed has an entry in [`SEEDS`] stating the control byte it carries and
//! the outcome it must produce; this binary fails when
//!
//! - a seed in the directory has no entry (an undocuments input),
//! - an entry has no seed (a dangling expectation),
//! - a seed's outcome differs from its expectation.
//!
//! Each file is `[control_byte][payload]`; the delivery and the parse come from
//! the `s3s_fuzz` harness, so this checks the seeds against the exact code the
//! fuzzer runs. Only the bits that change what the parser is handed (0x01, 0x02,
//! 0x10, 0x40) affect the outcome checked here: the seeds that ask for an extra
//! consumption strategy (0x04, 0x08, 0x20) are pinned by the primary path, and
//! the strategies themselves are covered by the target's oracles whenever the
//! corpus is fuzzed.
//!
//! Usage (from the repository root):
//! ```text
//! cargo run --manifest-path fuzz/Cargo.toml --bin check_seeds -- fuzz/seeds/multipart_parser
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use futures::executor::block_on;
use futures::stream;
use s3s_fuzz::{INJECTED_LIMIT, OutcomeKind, PartOutcome, max_buffer_size, run_full};
use s3s_multipart::{Boundary, Error, Multipart};

/// What a seed must produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// The parse ends cleanly after handing out these parts.
    Parts(&'static [(usize, usize)]),
    /// The parse fails with this error kind after handing out these parts.
    Fails(OutcomeKind, &'static [(usize, usize)]),
    /// The control byte asks for an injected error: the parse stops with
    /// `HeaderSizeExceeded`, and a parser handed only that error returns it
    /// unchanged instead of rewrapping it.
    Injected,
}

/// Every committed seed, sorted by name, with the outcome it is there for.
///
/// The parts are `(header count, data length)` pairs in the order they are
/// handed out.
const SEEDS: &[(&str, u8, Expect)] = &[
    // Malformed input: the parse must classify the failure, not guess.
    ("bad_header_line", 0x00, Expect::Fails(OutcomeKind::InvalidFormat, &[])),
    ("buffer_over", 0x10, Expect::Fails(OutcomeKind::HeaderSizeExceeded, &[])),
    ("missing_final_crlf", 0x00, Expect::Fails(OutcomeKind::IncompleteStream, &[(1, 4)])),
    // The part is still being read when the stream ends, so it is never handed
    // out: only parts that ran to completion are counted.
    ("truncated_body", 0x04, Expect::Fails(OutcomeKind::IncompleteStream, &[])),
    // A non-empty epilogue after the closing delimiter is rejected on purpose.
    ("epilogue", 0x20, Expect::Fails(OutcomeKind::StreamPartNotLast, &[(1, 4)])),
    // The stream's own error is handed back with its limit intact.
    ("injected_error", 0x01, Expect::Injected),
    // Boundaries of the header buffer.
    ("buffer_exact", 0x10, Expect::Parts(&[(1, 4)])),
    ("buffer_big_chunk", 0x12, Expect::Parts(&[(1, 4)])),
    ("small_buffer", 0x10, Expect::Fails(OutcomeKind::HeaderSizeExceeded, &[])),
    // Empty header blocks and empty parts are valid, in every position.
    ("empty_header_block", 0x00, Expect::Parts(&[(0, 6)])),
    ("empty_header_block_two_parts", 0x00, Expect::Parts(&[(1, 1), (0, 6)])),
    ("empty_body_part", 0x00, Expect::Parts(&[(1, 0)])),
    // A body delivered with an empty chunk between fragments still parses.
    ("empty_chunks", 0x40, Expect::Parts(&[(1, 4)])),
    ("empty_chunk_interleaved", 0x40, Expect::Parts(&[(0, 1200)])),
    // Well-formed bodies: one part, several parts, several headers, a preamble.
    ("valid_full", 0x00, Expect::Parts(&[(1, 5)])),
    ("valid_multiple_fields", 0x00, Expect::Parts(&[(1, 6), (1, 6), (1, 5)])),
    ("valid_many_headers", 0x00, Expect::Parts(&[(3, 5)])),
    ("valid_preamble_and_padding", 0x00, Expect::Parts(&[(1, 4)])),
    // The same body through the single-fragment, take and skip strategies.
    ("valid_single", 0x02, Expect::Parts(&[(1, 5)])),
    ("valid_take", 0x04, Expect::Parts(&[(1, 5)])),
    ("valid_take_strict", 0x24, Expect::Parts(&[(1, 5)])),
    ("valid_skip", 0x08, Expect::Parts(&[(1, 15)])),
];

fn main() -> ExitCode {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: check_seeds <seed-dir>");
        return ExitCode::FAILURE;
    };
    let dir = Path::new(&dir);

    let mut seeds = match read_seeds(dir) {
        Ok(seeds) => seeds,
        Err(err) => {
            eprintln!("cannot read {}: {err}", dir.display());
            return ExitCode::FAILURE;
        }
    };

    let mut failures = 0usize;
    for (name, control, expect) in SEEDS {
        let Some(path) = seeds.remove(*name) else {
            eprintln!("FAIL {name}: no seed file in {}", dir.display());
            failures += 1;
            continue;
        };
        match check(&path, *control, *expect) {
            Ok(outcome) => println!("ok   {name:<30} {outcome}"),
            Err(err) => {
                eprintln!("FAIL {name}: {err}");
                failures += 1;
            }
        }
    }
    for name in seeds.keys() {
        eprintln!("FAIL {name}: seed without an entry in SEEDS");
        failures += 1;
    }

    if failures == 0 {
        println!("checked {} seeds, all match", SEEDS.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("{failures} of {} seeds failed", SEEDS.len());
        ExitCode::FAILURE
    }
}

/// Reads `<dir>/*.bin`, keyed by file stem.
fn read_seeds(dir: &Path) -> std::io::Result<BTreeMap<String, PathBuf>> {
    let mut seeds = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "bin")
            && let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned())
        {
            seeds.insert(stem, path);
        }
    }
    Ok(seeds)
}

/// Parses one seed and reports the outcome it produced, or why it is wrong.
fn check(path: &Path, control: u8, expect: Expect) -> Result<String, String> {
    let data = fs::read(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let Some((&file_control, payload)) = data.split_first() else {
        return Err("empty seed: a seed is [control_byte][payload]".to_string());
    };
    if file_control != control {
        return Err(format!("control byte is 0x{file_control:02x}, expected 0x{control:02x}"));
    }

    let outcome = block_on(run_full(payload, control));
    let kind = outcome.kind;
    let shape: Vec<(usize, usize)> = outcome.parts.iter().map(shape_of).collect();
    let observed = format!("{kind:?} parts={}", show(&shape));

    let matched = match expect {
        Expect::Parts(want) => kind == OutcomeKind::Ok && shape == want,
        Expect::Fails(want_kind, want) => kind == want_kind && shape == want,
        Expect::Injected => kind == OutcomeKind::HeaderSizeExceeded && injected_error_identity().is_ok(),
    };
    if matched {
        return Ok(observed);
    }

    let want = match expect {
        Expect::Parts(want) => format!("Ok parts={}", show(want)),
        Expect::Fails(want_kind, want) => format!("{want_kind:?} parts={}", show(want)),
        Expect::Injected => match injected_error_identity() {
            Ok(()) => "HeaderSizeExceeded (injected error unchanged)".to_string(),
            Err(err) => return Err(format!("got {observed}, and {err}")),
        },
    };
    Err(format!("got {observed}, expected {want}"))
}

fn shape_of(part: &PartOutcome) -> (usize, usize) {
    (part.headers.len(), part.data.len())
}

fn show(shape: &[(usize, usize)]) -> String {
    let mut out = String::from("[");
    for (idx, (headers, data)) in shape.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("{headers}/{data}"));
    }
    out.push(']');
    out
}

/// Hands a parser nothing but the injected error: it must come back unchanged.
fn injected_error_identity() -> Result<(), String> {
    let items = vec![Err(Error::HeaderSizeExceeded { limit: INJECTED_LIMIT })];
    let mut multipart = Multipart::new(stream::iter(items), &Boundary::new(b"boundary").unwrap(), max_buffer_size(0));
    match block_on(async { multipart.next_part().await.err() }) {
        Some(Error::HeaderSizeExceeded { limit }) if limit == INJECTED_LIMIT => Ok(()),
        Some(err) => Err(format!("the injected error came back as {err}")),
        None => Err("the injected error was swallowed: next_part returned a part".to_string()),
    }
}
