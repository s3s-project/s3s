//! AWS restXml deserializer
//!
//! See <https://smithy.io/2.0/aws/protocols/aws-restxml-protocol.html#xml-shape-serialization>
//!

use crate::dto::{self, List, Timestamp, TimestampFormat};

use std::borrow::ToOwned;
use std::fmt;

use quick_xml::Reader;
use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

/// A data type that can be deserialized with AWS restXml deserializer
pub trait Deserialize<'xml>: Sized {
    /// Deserializes the data type
    ///
    /// # Errors
    /// Returns an error if the deserialization fails
    fn deserialize(d: &mut Deserializer<'xml>) -> DeResult<Self>;
}

/// A data type that can be deserialized with AWS restXml deserializer
pub trait DeserializeContent<'xml>: Sized {
    /// Deserializes the content of the data type
    ///
    /// # Errors
    /// Returns an error if the deserialization fails
    fn deserialize_content(d: &mut Deserializer<'xml>) -> DeResult<Self>;
}

/// AWS restXml deserializer
pub struct Deserializer<'xml> {
    /// xml reader
    inner: Reader<&'xml [u8]>,

    /// peeked event
    peeked: Option<DeEvent<'xml>>,

    /// store an extra event
    next_slot: Option<DeEvent<'xml>>,
}

/// XML deserialization result
pub type DeResult<T = (), E = DeError> = std::result::Result<T, E>;

/// XML deserialization error
#[derive(Debug, thiserror::Error)]
pub enum DeError {
    /// Invalid XML
    #[error("invalid XML: {0}")]
    InvalidXml(quick_xml::Error),

    /// Unexpected EOF
    #[error("unexpected eof")]
    UnexpectedEof,

    /// Unexpected start
    #[error("unexpected start")]
    UnexpectedStart,

    /// Unexpected end
    #[error("unexpected end")]
    UnexpectedEnd,

    /// Unexpected tag name
    #[error("unexpected tag name")]
    UnexpectedTagName,

    #[error("invalid attribute")]
    InvalidAttribute,

    #[error("unexpected attribute name")]
    UnexpectedAttributeName,

    /// Invalid content
    #[error("invalid content")]
    InvalidContent,

    /// Missing field
    #[error("missing field")]
    MissingField,

    /// Duplicate field
    #[error("duplicate field")]
    DuplicateField,
}

/// XML deserialization event
#[derive(Clone)]
enum DeEvent<'xml> {
    /// start
    Start(BytesStart<'xml>),
    /// end
    End(BytesEnd<'xml>),
    /// text
    Text(BytesText<'xml>),
    /// eof
    Eof,
}

impl<'xml> Deserializer<'xml> {
    /// Creates a new deserializer
    #[must_use]
    pub fn new(xml: &'xml [u8]) -> Self {
        Self {
            inner: Reader::from_reader(xml),
            peeked: None,
            next_slot: None,
        }
    }

    /// Reads the next event
    fn read_event(&mut self) -> DeResult<DeEvent<'xml>> {
        if let Some(ev) = self.next_slot.take() {
            return Ok(ev);
        }
        loop {
            let ev = self.inner.read_event().map_err(invalid_xml)?;
            let de = match ev {
                Event::Start(x) => DeEvent::Start(x),
                Event::End(x) => DeEvent::End(x),
                Event::Text(x) => DeEvent::Text(x),
                Event::Eof => DeEvent::Eof,

                Event::Empty(x) => {
                    // translate `<CSV/>` to `<CSV></CSV>`
                    self.next_slot = Some(DeEvent::End(x.to_end().into_owned()));
                    DeEvent::Start(x)
                }

                // expand XML entity / character references (e.g. &quot; → ", &#34; → ")
                // Note: resolve_xml_entity only covers the five predefined XML entities.
                // Numeric character references (&#NN; and &#xNN;) are handled by
                // resolve_char_ref below.
                // Using from_escaped() is the correct choice here because it allows
                // BytesText::decode() to properly reconstruct the value in the text accumulation logic.
                Event::GeneralRef(r) => {
                    let name = std::str::from_utf8(r.as_ref()).map_err(|_| DeError::InvalidContent)?;
                    let value: String = resolve_xml_entity(name)
                        .map(ToOwned::to_owned)
                        .or_else(|| resolve_char_ref(name))
                        .ok_or(DeError::InvalidContent)?;
                    DeEvent::Text(BytesText::from_escaped(value))
                }

                // ignore the others
                Event::Comment(_) | Event::CData(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) => continue,
            };
            break Ok(de);
        }
    }

    /// Returns the next event
    fn next_event(&mut self) -> DeResult<DeEvent<'xml>> {
        if let Some(ev) = self.peeked.take() {
            return Ok(ev);
        }
        self.read_event()
    }

    /// Peeks the next event
    #[allow(clippy::unwrap_used, clippy::unwrap_in_result)]
    fn peek_event(&mut self) -> DeResult<DeEvent<'xml>> {
        if self.peeked.is_none() {
            self.peeked = Some(self.read_event()?);
        }
        Ok(self.peeked.clone().unwrap())
    }

    /// Consumes the peeked event
    fn consume_peeked(&mut self) {
        self.peeked = None;
    }

    /// Expects a start event
    fn expect_start(&mut self, name: &[u8]) -> DeResult {
        loop {
            match self.next_event()? {
                DeEvent::Start(x) => {
                    if x.name().as_ref() != name {
                        return Err(unexpected_tag_name());
                    }
                    return Ok(());
                }
                DeEvent::End(_) => return Err(unexpected_end()),
                DeEvent::Text(_) => continue,
                DeEvent::Eof => return Err(unexpected_eof()),
            }
        }
    }

    /// Expects a start event with any of the given names.
    /// Returns the matched name (borrowed from `names`).
    fn expect_start_any<'s>(&mut self, names: &'s [&str]) -> DeResult<&'s str> {
        loop {
            match self.next_event()? {
                DeEvent::Start(x) => {
                    let name = x.name();
                    let name = name.as_ref();
                    for &n in names {
                        if n.as_bytes() == name {
                            return Ok(n);
                        }
                    }
                    return Err(unexpected_tag_name());
                }
                DeEvent::End(_) => return Err(unexpected_end()),
                DeEvent::Text(_) => continue,
                DeEvent::Eof => return Err(unexpected_eof()),
            }
        }
    }

    /// Expects an end event
    fn expect_end(&mut self, name: &[u8]) -> DeResult {
        loop {
            match self.next_event()? {
                DeEvent::Start(_) => return Err(unexpected_start()),
                DeEvent::End(x) => {
                    if x.name().as_ref() != name {
                        return Err(unexpected_tag_name());
                    }
                    return Ok(());
                }
                DeEvent::Text(_) => continue,
                DeEvent::Eof => return Err(unexpected_eof()),
            }
        }
    }

    /// Expects an eof event
    pub fn expect_eof(&mut self) -> DeResult {
        loop {
            match self.next_event()? {
                DeEvent::Start(_) => return Err(unexpected_start()),
                DeEvent::End(_) => return Err(unexpected_end()),
                DeEvent::Text(_) => continue,
                DeEvent::Eof => return Ok(()),
            }
        }
    }

    /// Deserializes an element
    ///
    /// # Errors
    /// Returns an error if the deserialization fails.
    pub fn named_element<T>(&mut self, name: &str, f: impl FnOnce(&mut Self) -> DeResult<T>) -> DeResult<T> {
        self.expect_start(name.as_bytes())?;
        let ans = f(self)?;
        self.expect_end(name.as_bytes())?;
        Ok(ans)
    }

    /// Deserializes an element with any of the given names.
    ///
    /// Unlike [`named_element`](Self::named_element), this method accepts
    /// multiple candidate root element names.  It consumes the start event
    /// (matching any of `names`), runs the content deserializer, and expects
    /// the corresponding end event.
    ///
    /// # Errors
    /// Returns an error if the deserialization fails.
    pub fn named_element_any<T>(&mut self, names: &[&str], f: impl FnOnce(&mut Self) -> DeResult<T>) -> DeResult<T> {
        debug_assert!(!names.is_empty(), "named_element_any requires at least one candidate name");
        let name = self.expect_start_any(names)?;
        let ans = f(self)?;
        self.expect_end(name.as_bytes())?;
        Ok(ans)
    }

    pub fn element<T>(&mut self, f: impl FnOnce(&mut Self, &[u8]) -> DeResult<T>) -> DeResult<T> {
        loop {
            match self.peek_event()? {
                DeEvent::Start(start) => {
                    self.consume_peeked();
                    let name = start.name();
                    let ans = f(self, name.as_ref())?;
                    self.expect_end(name.as_ref())?;
                    return Ok(ans);
                }
                DeEvent::Text(_) => {
                    self.consume_peeked();
                }
                DeEvent::End(_) | DeEvent::Eof => {
                    return Err(unexpected_end());
                }
            }
        }
    }

    /// Deserializes each element
    ///
    /// # Errors
    /// Returns an error if the deserialization fails.
    pub fn for_each_element(&mut self, mut f: impl FnMut(&mut Self, &[u8]) -> DeResult) -> DeResult {
        loop {
            match self.peek_event()? {
                DeEvent::Start(start) => {
                    self.consume_peeked();

                    let name = start.name();
                    let name = name.as_ref();
                    f(self, name)?;
                    self.expect_end(name)?;

                    continue;
                }
                DeEvent::Text(_) => {
                    self.consume_peeked();
                    continue;
                }
                DeEvent::End(_) | DeEvent::Eof => {
                    return Ok(());
                }
            }
        }
    }

    pub fn for_each_element_with_start(&mut self, mut f: impl FnMut(&mut Self, &[u8], &BytesStart<'_>) -> DeResult) -> DeResult {
        loop {
            match self.peek_event()? {
                DeEvent::Start(start) => {
                    self.consume_peeked();

                    let name = start.name();
                    let name = name.as_ref();
                    f(self, name, &start)?;
                    self.expect_end(name)?;

                    continue;
                }
                DeEvent::Text(_) => {
                    self.consume_peeked();
                    continue;
                }
                DeEvent::End(_) | DeEvent::Eof => {
                    return Ok(());
                }
            }
        }
    }

    /// Deserializes text
    ///
    /// Accumulates all consecutive text and entity-reference events into a single `&str`,
    /// correctly handling predefined XML entities (e.g. `&quot;` → `"`).
    ///
    /// # Errors
    /// Returns an error if the deserialization fails.
    pub fn text<T>(&mut self, f: impl FnOnce(&str) -> DeResult<T>) -> DeResult<T> {
        let mut buf: Option<String> = None;
        loop {
            match self.peek_event()? {
                DeEvent::Start(_) => {
                    self.consume_peeked();
                    return Err(unexpected_start());
                }
                // `End` terminates text accumulation without being consumed, so callers can
                // still match it (e.g. `expect_end`).
                DeEvent::End(_) => break,
                DeEvent::Eof => {
                    if buf.is_none() {
                        return Err(unexpected_eof());
                    }
                    break;
                }
                DeEvent::Text(x) => {
                    self.consume_peeked();
                    let s = x.decode().map_err(Into::into).map_err(invalid_xml)?;
                    match &mut buf {
                        None => buf = Some(s.into_owned()),
                        Some(b) => b.push_str(&s),
                    }
                }
            }
        }
        f(buf.as_deref().unwrap_or(""))
    }

    /// Deserializes the content of a field
    ///
    /// # Errors
    /// Returns an error if the deserialization fails.
    pub fn content<T: DeserializeContent<'xml>>(&mut self) -> DeResult<T> {
        T::deserialize_content(self)
    }

    pub fn list_content<T: DeserializeContent<'xml>>(&mut self, name: &str) -> DeResult<List<T>> {
        let mut list = List::new();
        self.for_each_element(|d, x| {
            if x != name.as_bytes() {
                // skip unknown elements for forward compatibility
                d.skip_element_content()?;
                return Ok(());
            }
            list.push(d.content()?);
            Ok(())
        })?;
        Ok(list)
    }

    /// Skips all remaining content inside the current element.
    ///
    /// This method should be called from within a [`for_each_element`] or
    /// [`for_each_element_with_start`] callback when an unrecognised element
    /// tag is encountered. It consumes all tokens (including nested elements)
    /// that belong to the current element, but does **not** consume the
    /// matching end tag — leaving it for the caller's [`expect_end`] to handle.
    ///
    /// The depth counter tracks nested child elements. When an `End` event is
    /// seen at `depth == 0` it belongs to the current (unrecognised) element
    /// and must not be consumed here; the surrounding [`for_each_element`] loop
    /// is responsible for consuming it via `expect_end`.
    ///
    /// # Errors
    /// Returns an error if an unexpected EOF is encountered before the end tag.
    pub fn skip_element_content(&mut self) -> DeResult {
        let mut depth: u32 = 0;
        loop {
            match self.peek_event()? {
                DeEvent::Start(_) => {
                    self.consume_peeked();
                    depth += 1;
                }
                DeEvent::End(_) => {
                    if depth == 0 {
                        // Do not consume: leave for the surrounding expect_end call.
                        return Ok(());
                    }
                    self.consume_peeked();
                    depth -= 1;
                }
                DeEvent::Text(_) => {
                    self.consume_peeked();
                }
                DeEvent::Eof => return Err(unexpected_eof()),
            }
        }
    }

    pub fn timestamp(&mut self, fmt: TimestampFormat) -> DeResult<Timestamp> {
        self.text(|s| Timestamp::parse(fmt, s).map_err(|_| DeError::InvalidContent))
    }
}

impl fmt::Debug for Deserializer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deserializer").finish_non_exhaustive()
    }
}

/// helper
const fn invalid_xml(err: quick_xml::Error) -> DeError {
    DeError::InvalidXml(err)
}

/// helper
const fn unexpected_eof() -> DeError {
    DeError::UnexpectedEof
}

/// helper
const fn unexpected_end() -> DeError {
    DeError::UnexpectedEnd
}

/// helper
const fn unexpected_tag_name() -> DeError {
    DeError::UnexpectedTagName
}

/// helper
const fn unexpected_start() -> DeError {
    DeError::UnexpectedStart
}

/// Resolves a numeric character reference such as `#34` (decimal) or `#x22` (hex).
///
/// Returns `None` if the input is not a valid character reference:
/// - missing `#` prefix
/// - empty number part
/// - invalid decimal/hex digits
/// - codepoint outside the valid Unicode range
/// - surrogate codepoints (`0xD800`–`0xDFFF`)
fn resolve_char_ref(name: &str) -> Option<String> {
    let entity = name.strip_prefix('#')?;
    if entity.is_empty() {
        return None;
    }
    let codepoint = if let Some(hex) = entity.strip_prefix('x') {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        entity.parse::<u32>().ok()?
    };

    // XML 1.0 valid character ranges:
    // (#x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] | [#x10000-#x10FFFF])
    if !matches!(
        codepoint,
        0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x0010_0000..=0x0010_FFFF
    ) {
        return None;
    }

    char::from_u32(codepoint).map(|c| c.to_string())
}

impl<'xml> DeserializeContent<'xml> for bool {
    fn deserialize_content(d: &mut Deserializer<'xml>) -> DeResult<Self> {
        d.text(|s| match s {
            "true" | "TRUE" => Ok(true),
            "false" | "FALSE" => Ok(false),
            _ => Err(DeError::InvalidContent),
        })
    }
}

impl<'xml> DeserializeContent<'xml> for String {
    fn deserialize_content(d: &mut Deserializer<'xml>) -> DeResult<Self> {
        d.text(|s| Ok(s.to_owned()))
    }
}

impl<'xml> DeserializeContent<'xml> for i32 {
    fn deserialize_content(d: &mut Deserializer<'xml>) -> DeResult<Self> {
        d.text(|s| atoi::atoi::<Self>(s.as_bytes()).ok_or(DeError::InvalidContent))
    }
}

impl<'xml> DeserializeContent<'xml> for i64 {
    fn deserialize_content(d: &mut Deserializer<'xml>) -> DeResult<Self> {
        d.text(|s| atoi::atoi::<Self>(s.as_bytes()).ok_or(DeError::InvalidContent))
    }
}

impl<'xml> DeserializeContent<'xml> for dto::Event {
    fn deserialize_content(d: &mut Deserializer<'xml>) -> DeResult<Self> {
        String::deserialize_content(d).map(Self::from)
    }
}
