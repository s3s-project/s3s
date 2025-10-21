//! Ordered headers

use hyper::HeaderMap;
use hyper::header::ToStrError;

use crate::utils::stable_sort_by_first;

use std::borrow::Cow;

/// Immutable http header container
#[derive(Debug, Default)]
pub struct OrderedHeaders<'a> {
    /// Ascending headers (header names are lowercase)
    /// Values can be either borrowed or owned (for UTF-8 metadata)
    headers: Vec<(&'a str, Cow<'a, str>)>,
}

impl<'a> OrderedHeaders<'a> {
    /// Constructs [`OrderedHeaders`] from slice
    ///
    /// + header names must be lowercase
    /// + header values must be valid
    #[cfg(test)]
    #[must_use]
    pub fn from_slice_unchecked(slice: &[(&'a str, &'a str)]) -> Self {
        for (name, _) in slice {
            let is_valid = |c: u8| c == b'-' || c.is_ascii_lowercase() || c.is_ascii_digit();
            assert!(name.as_bytes().iter().copied().all(is_valid));
        }
        let mut headers = Vec::new();
        for &(name, value) in slice {
            headers.push((name, Cow::Borrowed(value)));
        }
        stable_sort_by_first(&mut headers);
        Self { headers }
    }

    /// Constructs [`OrderedHeaders`] from a header map
    ///
    /// # Errors
    /// Returns [`ToStrError`] if header value cannot be converted to string slice
    pub fn from_headers(map: &'a HeaderMap) -> Result<Self, ToStrError> {
        let mut headers = Vec::with_capacity(map.len());

        for (name, value) in map {
            // First try to convert to ASCII str
            let value_cow = match value.to_str() {
                Ok(s) => Cow::Borrowed(s),
                Err(e) => {
                    // If that fails, try UTF-8 decoding for metadata headers
                    if name.as_str().starts_with("x-amz-meta-") {
                        // For metadata headers, decode as UTF-8
                        let utf8_str = std::str::from_utf8(value.as_bytes())
                            .map_err(|_| e)?;
                        Cow::Owned(utf8_str.to_owned())
                    } else {
                        return Err(e);
                    }
                }
            };
            headers.push((name.as_str(), value_cow));
        }
        stable_sort_by_first(&mut headers);

        Ok(Self { headers })
    }

    fn get_all_pairs(&self, name: &str) -> impl Iterator<Item = (&'a str, &str)> + '_ {
        let slice = self.headers.as_slice();

        let lower_bound = slice.partition_point(|x| x.0 < name);
        let upper_bound = slice.partition_point(|x| x.0 <= name);

        slice[lower_bound..upper_bound].iter().map(|(n, v)| (*n, v.as_ref()))
    }

    pub fn get_all(&self, name: impl AsRef<str>) -> impl Iterator<Item = &str> + '_ {
        self.get_all_pairs(name.as_ref()).map(|x| x.1)
    }

    fn get_unique_pair(&self, name: &'_ str) -> Option<(&'a str, &str)> {
        let slice = self.headers.as_slice();
        let lower_bound = slice.partition_point(|x| x.0 < name);

        let mut iter = slice[lower_bound..].iter();
        let (n, v) = iter.next()?;

        if let Some((following_n, _)) = iter.next() {
            if following_n == &name {
                return None;
            }
        }

        (*n == name).then_some((*n, v.as_ref()))
    }

    /// Gets header value by name. Time `O(logn)`
    pub fn get_unique(&self, name: impl AsRef<str>) -> Option<&str> {
        self.get_unique_pair(name.as_ref()).map(|(_, v)| v)
    }

    // /// Finds headers by names. Time `O(mlogn)`
    // #[must_use]
    // pub fn find_multiple(&self, names: &[impl AsRef<str>]) -> Self {
    //     let mut headers: Vec<(&'a str, &'a str)> = Vec::new();
    //     for name in names {
    //         for pair in self.get_all_pairs(name.as_ref()) {
    //             headers.push(pair);
    //         }
    //     }
    //     Self { headers }
    // }

    /// Finds headers by names. Time `O(mlogn)`
    #[must_use]
    pub fn find_multiple_with_on_missing(
        &self,
        names: &'a [impl AsRef<str>],
        on_missing: impl Fn(&'a str) -> Option<&'a str>,
    ) -> Self {
        let mut headers = Vec::new();
        for name in names {
            let mut has_value = false;
            for (n, v) in self.get_all_pairs(name.as_ref()) {
                headers.push((n, Cow::Borrowed(v)));
                has_value = true;
            }
            if !has_value {
                if let Some(value) = on_missing(name.as_ref()) {
                    headers.push((name.as_ref(), Cow::Borrowed(value)));
                }
            }
        }
        Self { headers }
    }
    
    /// Returns an iterator over (name, value) pairs as (&str, &str)
    pub fn iter_pairs(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.headers.iter().map(|(n, v)| (*n, v.as_ref()))
    }
}

impl<'a> AsRef<[(&'a str, Cow<'a, str>)]> for OrderedHeaders<'a> {
    fn as_ref(&self) -> &[(&'a str, Cow<'a, str>)] {
        self.headers.as_ref()
    }
}
