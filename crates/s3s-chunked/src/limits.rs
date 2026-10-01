// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

/// Decoding limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum size of a chunk metadata line, in bytes.
    pub max_chunk_meta_size: usize,

    /// Maximum size of a chunk that carries a chunk signature, in bytes.
    ///
    /// A signed chunk is buffered in memory until its signature is verified, so
    /// this limit bounds the payload bytes retained for it. Fragments are kept
    /// as delivered while they are whole read buffers, and coalesced into one
    /// allocation once they are small or numerous, which also bounds the
    /// per-fragment overhead a malformed request can pin. Unsigned chunks are
    /// streamed through without buffering and are not limited here.
    pub max_signed_chunk_size: usize,

    /// Maximum size of the trailer block, in bytes.
    pub max_trailers_size: usize,

    /// Maximum number of trailer headers.
    pub max_trailer_headers: usize,
}

impl Limits {
    /// Default maximum size of a chunk metadata line, in bytes.
    pub const DEFAULT_MAX_CHUNK_META_SIZE: usize = 1024;

    /// Default maximum size of a signed chunk, in bytes (256 MiB).
    pub const DEFAULT_MAX_SIGNED_CHUNK_SIZE: usize = 256 * 1024 * 1024;

    /// Default maximum size of the trailer block, in bytes (16 KiB).
    pub const DEFAULT_MAX_TRAILERS_SIZE: usize = 16 * 1024;

    /// Default maximum number of trailer headers.
    pub const DEFAULT_MAX_TRAILER_HEADERS: usize = 100;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_chunk_meta_size: Self::DEFAULT_MAX_CHUNK_META_SIZE,
            max_signed_chunk_size: Self::DEFAULT_MAX_SIGNED_CHUNK_SIZE,
            max_trailers_size: Self::DEFAULT_MAX_TRAILERS_SIZE,
            max_trailer_headers: Self::DEFAULT_MAX_TRAILER_HEADERS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_match_the_historical_constants() {
        let limits = Limits::default();
        assert_eq!(limits.max_chunk_meta_size, 1024);
        assert_eq!(limits.max_signed_chunk_size, 256 * 1024 * 1024);
        assert_eq!(limits.max_trailers_size, 16 * 1024);
        assert_eq!(limits.max_trailer_headers, 100);
    }
}
