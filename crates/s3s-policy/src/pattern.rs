// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

pub struct PatternSet {
    // TODO: rewrite the naive implementation with something like Aho-Corasick
    patterns: Vec<Pattern>,
}

#[derive(Debug, thiserror::Error)]
pub enum PatternError {
    #[error("Invalid pattern")]
    InvalidPattern,
}

#[derive(Debug)]
struct Pattern {
    bytes: Vec<u8>,
}

impl PatternSet {
    /// Create a new matcher from a list of patterns.
    ///
    /// Patterns can contain
    /// + `*` to match any sequence of characters (including empty sequence)
    /// + `?` to match any single character
    /// + any other character to match itself
    ///
    /// # Errors
    /// Returns an error if any pattern is invalid.
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<PatternSet, PatternError> {
        let patterns = patterns.into_iter().map(Self::parse_pattern).collect::<Result<_, _>>()?;
        Ok(PatternSet { patterns })
    }

    fn parse_pattern(pattern: &str) -> Result<Pattern, PatternError> {
        if pattern.is_empty() {
            return Err(PatternError::InvalidPattern);
        }
        Ok(Pattern {
            bytes: pattern.as_bytes().to_owned(),
        })
    }

    /// Check if the input matches any of the patterns.
    #[must_use]
    pub fn is_match(&self, input: &str) -> bool {
        for pattern in &self.patterns {
            if Self::match_pattern(&pattern.bytes, input.as_bytes()) {
                return true;
            }
        }
        false
    }

    /// <https://leetcode.com/problems/wildcard-matching/>
    fn match_pattern(pattern: &[u8], input: &[u8]) -> bool {
        let mut p_idx = 0;
        let mut s_idx = 0;

        let mut p_back = usize::MAX - 1;
        let mut s_back = usize::MAX - 1;

        loop {
            if p_idx < pattern.len() {
                let p = pattern[p_idx];
                if p == b'*' {
                    p_idx += 1;
                    p_back = p_idx;
                    s_back = s_idx;
                    continue;
                }

                if s_idx < input.len() {
                    let c = input[s_idx];
                    if p == c || p == b'?' {
                        p_idx += 1;
                        s_idx += 1;
                        continue;
                    }
                }
            } else if s_idx == input.len() {
                return true;
            }

            if p_back == pattern.len() {
                return true;
            }

            if s_back + 1 < input.len() {
                s_back += 1;
                p_idx = p_back;
                s_idx = s_back;
                continue;
            }

            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match() {
        let cases = &[
            ("*", "", true),
            ("**", "", true),
            ("***", "abc", true),
            ("a", "aa", false),
            ("***a", "aaaa", true),
            ("*abc???def", "abcdefabc123def", true),
            ("a*c?b", "acdcb", false),
            ("*a*b*c*", "abc", true),
            ("a*b*c*", "abc", true),
            ("*a*b*c", "abc", true),
            ("a*b*c", "abc", true),
        ];

        for &(pattern, input, expected) in cases {
            let pattern = PatternSet::parse_pattern(pattern).unwrap();
            let ans = PatternSet::match_pattern(&pattern.bytes, input.as_bytes());
            assert_eq!(ans, expected, "pattern: {pattern:?}, input: {input:?}");
        }
    }
}
