// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

//! Boundary delimiter patterns and the data-phase search for them.

use memchr::memmem;

/// Chunks at or below this size keep the old fixed-tail behavior.
const ADAPTIVE_TAIL_THRESHOLD: usize = 2048;

#[derive(Debug, Clone, Copy)]
pub enum DataSearch {
    Found { index: usize },
    Emit { end: usize },
    KeepAll,
}

pub fn search_data(haystack: &[u8], delimiter_finder: &memmem::Finder<'static>) -> DataSearch {
    if let Some(index) = delimiter_finder.find(haystack) {
        return DataSearch::Found { index };
    }

    let delimiter = delimiter_finder.needle();
    let keep = delimiter.len().saturating_sub(1);
    let retain = if haystack.len() > ADAPTIVE_TAIL_THRESHOLD {
        // Keep only the longest suffix that could become a delimiter prefix.
        // The check is limited to the last delimiter-1 bytes and uses a cheap
        // first-byte prefilter before doing any slice comparison.
        delimiter_prefix_suffix_len(haystack, delimiter)
    } else {
        keep
    };
    if haystack.len() > retain {
        DataSearch::Emit {
            end: haystack.len() - retain,
        }
    } else {
        DataSearch::KeepAll
    }
}

fn delimiter_prefix_suffix_len(haystack: &[u8], delimiter: &[u8]) -> usize {
    // Unreachable: `make_delimiter` always produces `\r\n--` plus at least one
    // boundary byte, so the delimiter is never empty. The guard keeps an empty
    // slice out of the window arithmetic below.
    let Some(&first) = delimiter.first() else {
        return 0;
    };
    let max = delimiter.len().saturating_sub(1);
    let start = haystack.len().saturating_sub(max);
    let window = &haystack[start..];
    if memchr::memchr(first, window).is_none() {
        return 0;
    }
    for rel in memchr::memchr_iter(first, window) {
        let idx = start.saturating_add(rel);
        let len = haystack.len() - idx;
        if len <= max && haystack[idx..] == delimiter[..len] {
            return len;
        }
    }
    0
}

pub fn make_first_boundary(boundary: &[u8]) -> Box<[u8]> {
    let mut pattern = Vec::with_capacity(boundary.len().saturating_add(2));
    pattern.extend_from_slice(b"--");
    pattern.extend_from_slice(boundary);
    pattern.into_boxed_slice()
}

pub fn make_delimiter(boundary: &[u8]) -> Box<[u8]> {
    let mut pattern = Vec::with_capacity(boundary.len().saturating_add(4));
    pattern.extend_from_slice(b"\r\n--");
    pattern.extend_from_slice(boundary);
    pattern.into_boxed_slice()
}

pub fn make_delimiter_finder(boundary: &[u8]) -> Box<memmem::Finder<'static>> {
    // Build the searcher once so data-path chunks do not reconstruct it on
    // every call. Boxing keeps the parser state compact for s3s future sizes.
    Box::new(memmem::FinderBuilder::new().build_forward_owned(make_delimiter(boundary)))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    /// `\r\n--boundary` is twelve bytes, so a proper prefix is at most eleven.
    const DELIMITER: &[u8] = b"\r\n--boundary";

    fn suffix_len(haystack: &[u8]) -> usize {
        delimiter_prefix_suffix_len(haystack, DELIMITER)
    }

    /// The tail analysis must not depend on one delimiter length. For several
    /// boundary lengths — the shortest, the longest a boundary may be (70
    /// characters), and a few in between — every proper prefix of the delimiter
    /// is measured exactly and non-matching tails are not held back, against a
    /// brute-force reference.
    #[test]
    fn delimiter_prefix_suffix_len_handles_every_delimiter_length() {
        /// The longest proper prefix of `delimiter` that is a suffix of the
        /// haystack, computed by trying every length.
        fn oracle(haystack: &[u8], delimiter: &[u8]) -> usize {
            let max = delimiter.len().saturating_sub(1).min(haystack.len());
            (1..=max)
                .rev()
                .find(|&len| haystack[haystack.len() - len..] == delimiter[..len])
                .unwrap_or(0)
        }

        for boundary_len in [1usize, 2, 3, 7, 11, 12, 13, 68, 70] {
            const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
            let boundary: Vec<u8> = (0..boundary_len).map(|idx| ALPHABET[idx % ALPHABET.len()]).collect();
            let delimiter = make_delimiter(&boundary);
            let mut cases = vec![Vec::new(), b"data".to_vec(), delimiter.to_vec()];

            // Every proper prefix, alone and doubled.
            for len in 1..delimiter.len() {
                let mut single = b"data".to_vec();
                single.extend_from_slice(&delimiter[..len]);
                cases.push(single);

                let mut doubled = b"data".to_vec();
                doubled.extend_from_slice(&delimiter[..len]);
                doubled.extend_from_slice(&delimiter[..len]);
                cases.push(doubled);
            }

            // A tail that starts like the delimiter without being a prefix of it.
            let mut near_miss = b"data".to_vec();
            near_miss.extend_from_slice(&delimiter[..delimiter.len() - 1]);
            near_miss.push(b'!');
            cases.push(near_miss);

            for haystack in cases {
                assert_eq!(
                    delimiter_prefix_suffix_len(&haystack, &delimiter),
                    oracle(&haystack, &delimiter),
                    "boundary_len={boundary_len} haystack={haystack:?}"
                );
            }
        }
    }

    /// F31: every proper prefix of the delimiter must be measured exactly, so a
    /// chunk boundary that falls inside the delimiter keeps those bytes back.
    #[test]
    fn delimiter_prefix_suffix_len_measures_every_proper_prefix() {
        for len in 1..DELIMITER.len() {
            let mut haystack = b"data".to_vec();
            haystack.extend_from_slice(&DELIMITER[..len]);
            assert_eq!(suffix_len(&haystack), len, "prefix len={len}");
        }
        // As a whole suffix the delimiter is longer than `max`, and its first
        // byte falls outside the window; `search_data` never asks this anyway
        // because a complete delimiter short-circuits to `Found`.
        assert_eq!(suffix_len(DELIMITER), 0);
    }

    /// F31: tails that merely look similar must not be held back, and the
    /// first-byte prefilter must not turn a non-match into a match.
    #[test]
    fn delimiter_prefix_suffix_len_rejects_non_matching_tails() {
        assert_eq!(suffix_len(b""), 0);
        assert_eq!(suffix_len(b"data"), 0);
        // Ends with the delimiter's first byte, but not with a prefix of it.
        assert_eq!(suffix_len(b"\r\r\r\r\r"), 1);
        // Longer than `max` and only the last byte matches the prefix start.
        assert_eq!(suffix_len(b"data\r-\r"), 1);
        // A false prefix (`\r\n--C` does not start `\r\n--boundary`).
        assert_eq!(suffix_len(b"data\r\n--C"), 0);
        // No first byte anywhere in the window: prefilter returns early.
        assert_eq!(suffix_len(b"datahello"), 0);
    }

    /// F31: the adaptive tail only kicks in above the threshold. Exactly
    /// `ADAPTIVE_TAIL_THRESHOLD` bytes keep the fixed `delimiter - 1` tail,
    /// one byte more switches to the computed suffix.
    #[test]
    fn search_data_switches_to_the_adaptive_tail_above_the_threshold() {
        let finder = make_delimiter_finder(b"boundary");
        let keep = DELIMITER.len() - 1;

        let at_threshold = vec![b'x'; ADAPTIVE_TAIL_THRESHOLD];
        assert!(matches!(
            search_data(&at_threshold, &finder),
            DataSearch::Emit { end } if end == ADAPTIVE_TAIL_THRESHOLD - keep
        ));

        let above_threshold = vec![b'x'; ADAPTIVE_TAIL_THRESHOLD + 1];
        assert!(matches!(
            search_data(&above_threshold, &finder),
            DataSearch::Emit { end } if end == ADAPTIVE_TAIL_THRESHOLD + 1
        ));
    }

    /// F31: a large haystack that ends with a delimiter prefix emits everything
    /// before that prefix (the plain fixed tail would hold back more).
    #[test]
    fn search_data_keeps_only_the_matching_suffix_of_a_large_haystack() {
        let finder = make_delimiter_finder(b"boundary");

        let mut large = vec![b'x'; ADAPTIVE_TAIL_THRESHOLD * 2];
        large.extend_from_slice(&DELIMITER[..5]);
        assert!(matches!(
            search_data(&large, &finder),
            DataSearch::Emit { end } if end == ADAPTIVE_TAIL_THRESHOLD * 2
        ));

        // Below the threshold the fixed tail is kept instead.
        let mut small = vec![b'x'; 32];
        small.extend_from_slice(&DELIMITER[..5]);
        assert!(matches!(
            search_data(&small, &finder),
            DataSearch::Emit { end } if end == 32 + 5 - (DELIMITER.len() - 1)
        ));
    }

    /// F40: `Emit` always hands the caller at least one byte and never more than
    /// the haystack — the caller emits `haystack[..end]` and keeps the rest, so
    /// an `end` of zero would leave its `while let Some(chunk)` loop spinning
    /// without progress. The decision is checked against a reference written
    /// straight from the contract (find, else emit all but the longest
    /// delimiter-prefix suffix, else keep everything), over haystack lengths
    /// around `ADAPTIVE_TAIL_THRESHOLD` and tails that reach the suffix
    /// analysis.
    #[test]
    fn search_data_matches_the_retention_oracle() {
        /// Compares results without requiring `PartialEq` on `DataSearch`.
        fn key(search: DataSearch) -> (u8, usize) {
            match search {
                DataSearch::Found { index } => (0, index),
                DataSearch::Emit { end } => (1, end),
                DataSearch::KeepAll => (2, 0),
            }
        }

        /// Reference: find, else emit all but the longest proper delimiter
        /// prefix (or the fixed tail below the threshold), else keep all.
        fn oracle(haystack: &[u8], delimiter: &[u8], keep: usize) -> DataSearch {
            if let Some(index) = memmem::find(haystack, delimiter) {
                return DataSearch::Found { index };
            }
            let retain = if haystack.len() > ADAPTIVE_TAIL_THRESHOLD {
                let max = keep.min(haystack.len());
                (1..=max)
                    .rev()
                    .find(|&len| haystack[haystack.len() - len..] == delimiter[..len])
                    .unwrap_or(0)
            } else {
                keep
            };
            if haystack.len() > retain {
                DataSearch::Emit {
                    end: haystack.len() - retain,
                }
            } else {
                DataSearch::KeepAll
            }
        }

        let finder = make_delimiter_finder(b"boundary");
        let keep = DELIMITER.len() - 1;

        let tails: [&[u8]; 7] = [b"", b"x", b"\r", b"\r\n", &DELIMITER[..4], &DELIMITER[..keep], DELIMITER];
        let lens = [
            0,
            1,
            keep - 1,
            keep,
            keep + 1,
            64,
            ADAPTIVE_TAIL_THRESHOLD - 1,
            ADAPTIVE_TAIL_THRESHOLD,
            ADAPTIVE_TAIL_THRESHOLD + 1,
            ADAPTIVE_TAIL_THRESHOLD * 2,
        ];

        for len in lens {
            for tail in tails {
                let mut haystack = vec![b'x'; len];
                haystack.extend_from_slice(tail);
                let got = search_data(&haystack, &finder);

                // Invariants first, so a violation names the shape rather than
                // only the mismatch.
                if let DataSearch::Emit { end } = got {
                    assert!(end >= 1, "an emit must carry data: len={len} tail={tail:?}");
                    assert!(end <= haystack.len(), "an emit must stay in range: len={len} tail={tail:?}");
                }
                if let DataSearch::Found { index } = got {
                    assert_eq!(
                        &haystack[index..index + DELIMITER.len()],
                        DELIMITER,
                        "a find must be exact: len={len} tail={tail:?}"
                    );
                }

                let want = oracle(&haystack, DELIMITER, keep);
                assert_eq!(key(got), key(want), "len={len} tail={tail:?} haystack={haystack:?}");
            }
        }
    }

    /// F31: a complete delimiter short-circuits to `Found`, and a haystack
    /// shorter than the tail is kept whole.
    #[test]
    fn search_data_reports_found_and_keep_all() {
        let finder = make_delimiter_finder(b"boundary");

        let mut with_delimiter = vec![b'x'; 40];
        with_delimiter.extend_from_slice(DELIMITER);
        assert!(matches!(
            search_data(&with_delimiter, &finder),
            DataSearch::Found { index } if index == 40
        ));

        assert!(matches!(search_data(b"abc", &finder), DataSearch::KeepAll));
        assert!(matches!(search_data(b"", &finder), DataSearch::KeepAll));
    }
}
