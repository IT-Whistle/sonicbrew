//! M10 — PulseAudio native-protocol **tagstruct** encoding and decoding.
//!
//! The P1 codec ([`crate::codec`]) parses the 20-byte frame descriptor, a
//! [`SampleSpec`](crate::codec::SampleSpec) and length-prefixed strings. The
//! daemon handshake additionally needs *tagstructs*: the typed, tagged
//! payload carried inside each frame body after the `u32` command and `u32`
//! tag words. Every field is prefixed with a one-byte tag; integers and
//! length prefixes are big-endian (the same conventions PulseAudio's own
//! `pulsecore/tagstruct.c` uses; the tag bytes below are that file's).
//!
//! ```text
//!  tag  type           payload
//!  ---- -------------- -----------------------------------------------
//!  0x01 U32            u32
//!  0x02 U8             u8
//!  0x03 BOOLEAN_TRUE   (none)
//!  0x04 BOOLEAN_FALSE  (none)
//!  0x05 STRING         u32 length (incl. NUL) + bytes + NUL
//!  0x06 STRING_NULL    (none)
//!  0x07 ARBITRARY      u32 length + raw bytes
//!  0x08 U64            u64
//!  0x0B PROPLIST       (STRING key + STRING value)* then STRING_NULL
//! ```
//!
//! A few reply fields are carried **raw** (no tag byte): the server-info
//! sample spec (6 bytes: `u32` rate, `u8` channels, `u8` format) and the
//! trailing channel map (`u8` count + count position bytes). Those are
//! handled by [`TagReader::read_sample_spec`] / [`TagReader::skip_channel_map`]
//! and mirrored by [`TagWriter::sample_spec`] / [`TagWriter::channel_map`].

use crate::codec::{PulseError, MAX_LEN};

/// `PA_TAG_U32` — `u32` payload.
pub const TAG_U32: u8 = b'L';
/// `PA_TAG_U8` — `u8` payload.
pub const TAG_U8: u8 = b'B';
/// `PA_TAG_BOOLEAN_TRUE` — no payload.
pub const TAG_BOOLEAN_TRUE: u8 = b'1';
/// `PA_TAG_BOOLEAN_FALSE` — no payload.
pub const TAG_BOOLEAN_FALSE: u8 = b'0';
/// `PA_TAG_STRING` — length-prefixed, NUL-terminated string.
pub const TAG_STRING: u8 = b't';
/// `PA_TAG_STRING_NULL` — absent/optional string.
pub const TAG_STRING_NULL: u8 = b'N';
/// `PA_TAG_ARBITRARY` — length-prefixed raw bytes (e.g. the auth cookie).
pub const TAG_ARBITRARY: u8 = b'x';
/// `PA_TAG_U64` — `u64` payload.
pub const TAG_U64: u8 = b'R';
/// `PA_TAG_PROPLIST` — key/value string pairs, terminated by
/// [`TAG_STRING_NULL`].
pub const TAG_PROPLIST: u8 = b'P';
/// `PA_TAG_VOLUME` — `u32` payload (`pa_volume_t`, e.g. `PA_VOLUME_NORM`).
pub const TAG_VOLUME: u8 = b'v';
/// `PA_TAG_SAMPLE_SPEC` — `u32` rate + `u8` channels + `u8` format payload.
pub const TAG_SAMPLE_SPEC: u8 = b'a';
/// `PA_TAG_CHANNEL_MAP` — `u8` count + position bytes payload.
pub const TAG_CHANNEL_MAP: u8 = b'm';
/// `PA_TAG_CVOLUME` — raw `u8` count + raw `u32` volumes payload.
pub const TAG_CVOLUME: u8 = b'v';

/// Incremental tagstruct **writer** (serialize side of the codec).
///
/// Builds the body bytes that follow the `u32` command + `u32` tag words of
/// a native-protocol frame; see [`crate::daemon`] for framing.
#[derive(Debug, Default)]
pub struct TagWriter {
    buf: Vec<u8>,
}

impl TagWriter {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a tagged `u32`.
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.buf.push(TAG_U32);
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a tagged `u8`.
    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(TAG_U8);
        self.buf.push(value);
        self
    }

    /// Appends a tagged boolean (TRUE/FALSE tags carry no payload).
    pub fn boolean(&mut self, value: bool) -> &mut Self {
        self.buf.push(if value {
            TAG_BOOLEAN_TRUE
        } else {
            TAG_BOOLEAN_FALSE
        });
        self
    }

    /// Appends a tagged string: `'t'` + UTF-8 bytes + NUL. PA 17's
    /// `pa_tagstruct_puts` writes NO length prefix — readers scan for the
    /// NUL terminator.
    pub fn string(&mut self, value: &str) -> &mut Self {
        self.buf.push(TAG_STRING);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
        self
    }

    /// Appends a tagged NULL string (terminates proplists and marks absent
    /// optional strings).
    pub fn string_null(&mut self) -> &mut Self {
        self.buf.push(TAG_STRING_NULL);
        self
    }

    /// Appends a tagged arbitrary blob (length-prefixed raw bytes, no NUL).
    pub fn arbitrary(&mut self, data: &[u8]) -> &mut Self {
        self.buf.push(TAG_ARBITRARY);
        let len = u32::try_from(data.len()).expect("blob length fits in u32");
        self.buf.extend_from_slice(&len.to_be_bytes());
        self.buf.extend_from_slice(data);
        self
    }

    /// Appends a tagged `u64`.
    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.buf.push(TAG_U64);
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a tagged proplist (PA 17 wire format): `'P'`, then for each
    /// pair a string KEY followed by the value as `u32(len)` + `arbitrary`
    /// (the double length encoding is what `pa_tagstruct_get_proplist`
    /// parses: gets(key) → getu32(len) → get_arbitrary(len)), then a
    /// terminating NULL string.
    pub fn proplist(&mut self, pairs: &[(&str, &str)]) -> &mut Self {
        self.buf.push(TAG_PROPLIST);
        for (key, value) in pairs {
            self.string(key);
            self.u32(u32::try_from(value.len()).expect("value length fits in u32"));
            self.arbitrary(value.as_bytes());
        }
        self.string_null();
        self
    }

    /// Appends a tagged 6-byte sample spec (`'a'` + `u32` rate, `u8`
    /// channels, `u8` format). The format byte is written verbatim.
    pub fn sample_spec(&mut self, sample_rate: u32, channels: u8, format: u8) -> &mut Self {
        self.buf.push(TAG_SAMPLE_SPEC);
        // PA field order: format(u8), channels(u8), rate(u32 BE).
        self.buf.push(format);
        self.buf.push(channels);
        self.buf.extend_from_slice(&sample_rate.to_be_bytes());
        self
    }

    /// Appends a tagged channel map (`'m'` + `u8` position count + one
    /// position byte per channel).
    pub fn channel_map(&mut self, positions: &[u8]) -> &mut Self {
        self.buf.push(TAG_CHANNEL_MAP);
        let count = u8::try_from(positions.len()).expect("channel count fits in u8");
        self.buf.push(count);
        self.buf.extend_from_slice(positions);
        self
    }

    /// Appends a tagged volume (`'v'` + `u32`, `pulse/def.h`'s
    /// `pa_volume_t`; `PA_VOLUME_NORM` = `0x10000`).
    pub fn volume(&mut self, value: u32) -> &mut Self {
        self.buf.push(TAG_VOLUME);
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    /// Appends a tagged cvolume — PA 17 wire format (`put_cvolume`): `'v'`
    /// tag, then a RAW `u8` channel count (no 'B' tag!) and RAW `u32` BE
    /// volumes (no 'V' tags — asymmetric with the reader, which is what the
    /// C source does).
    pub fn cvolume(&mut self, volumes: &[u32]) -> &mut Self {
        self.buf.push(TAG_CVOLUME);
        let count = u8::try_from(volumes.len()).expect("cvolume channel count fits in u8");
        self.buf.push(count); // raw count — NOT a tagged u8
        for value in volumes {
            self.buf.extend_from_slice(&value.to_be_bytes()); // raw u32
        }
        self
    }

    /// Consumes the writer and returns the serialized tagstruct bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Incremental tagstruct **reader** (deserialize side of the codec).
///
/// Reads typed fields from a borrowed byte slice, advancing a cursor.
/// Construction is infallible; every read validates the expected tag byte
/// and the payload length, returning [`PulseError::Truncated`] /
/// [`PulseError::InvalidTag`] / [`PulseError::Oversized`] /
/// [`PulseError::InvalidUtf8`] as appropriate.
#[derive(Debug)]
pub struct TagReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> TagReader<'a> {
    /// Current byte offset into the underlying buffer (after consumed tags).
    #[must_use]
    pub fn offset(&self) -> usize {
        self.pos
    }

    /// Creates a reader over a complete tagstruct byte slice.
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether every input byte has been consumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PulseError> {
        let end = self.pos.checked_add(n).ok_or(PulseError::Oversized)?;
        if end > self.buf.len() {
            return Err(PulseError::Truncated);
        }
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn peek_byte(&self) -> Result<u8, PulseError> {
        self.buf.get(self.pos).copied().ok_or(PulseError::Truncated)
    }

    fn expect_tag(&mut self, expected: u8) -> Result<(), PulseError> {
        let byte = self.take(1)?[0];
        if byte != expected {
            return Err(PulseError::InvalidTag(byte));
        }
        Ok(())
    }

    /// Reads a tagged `u32`.
    pub fn read_u32(&mut self) -> Result<u32, PulseError> {
        self.expect_tag(TAG_U32)?;
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a tagged `u8`.
    pub fn read_u8(&mut self) -> Result<u8, PulseError> {
        self.expect_tag(TAG_U8)?;
        Ok(self.take(1)?[0])
    }

    /// Reads a tagged boolean.
    pub fn read_boolean(&mut self) -> Result<bool, PulseError> {
        let byte = self.take(1)?[0];
        match byte {
            TAG_BOOLEAN_TRUE => Ok(true),
            TAG_BOOLEAN_FALSE => Ok(false),
            other => Err(PulseError::InvalidTag(other)),
        }
    }

    /// Reads a tagged string. `TAG_STRING` yields `Some`, `TAG_STRING_NULL`
    /// yields `None` (the wire encoding of an absent optional string).
    pub fn read_string(&mut self) -> Result<Option<String>, PulseError> {
        let byte = self.take(1)?[0];
        match byte {
            TAG_STRING_NULL => Ok(None),
            TAG_STRING => {
                // PA 17 wire format: NO length prefix — scan for the NUL
                // terminator (exactly what `pa_tagstruct_gets` does).
                let nul = self.buf[self.pos..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or(PulseError::Truncated)?;
                let end = self.pos + nul;
                let bytes = &self.buf[self.pos..end];
                if nul > MAX_LEN as usize {
                    return Err(PulseError::Oversized);
                }
                let s = std::str::from_utf8(bytes).map_err(|_| PulseError::InvalidUtf8)?;
                self.pos = end + 1; // consume bytes + NUL
                Ok(Some(s.to_owned()))
            }
            other => Err(PulseError::InvalidTag(other)),
        }
    }

    /// Reads a tagged arbitrary blob (borrowed from the input).
    pub fn read_arbitrary(&mut self) -> Result<&'a [u8], PulseError> {
        self.expect_tag(TAG_ARBITRARY)?;
        let len_bytes = self.take(4)?;
        let len = u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]);
        if len > MAX_LEN {
            return Err(PulseError::Oversized);
        }
        self.take(usize::try_from(len).expect("len <= MAX_LEN fits usize"))
    }

    /// Reads a tagged `u64`.
    pub fn read_u64(&mut self) -> Result<u64, PulseError> {
        self.expect_tag(TAG_U64)?;
        let bytes = self.take(8)?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(raw))
    }

    /// Reads a tagged proplist (PA 17 wire format): each entry is a string
    /// KEY followed by `u32(len)` + `arbitrary(len)` for the value,
    /// terminated by a NULL string.
    pub fn read_proplist(&mut self) -> Result<Vec<(String, String)>, PulseError> {
        self.expect_tag(TAG_PROPLIST)?;
        let mut pairs = Vec::new();
        loop {
            match self.peek_byte()? {
                TAG_STRING => {
                    let key = self.read_string()?.unwrap_or_default();
                    // Value: tagged u32 length + tagged arbitrary blob.
                    let len = self.read_u32()? as usize;
                    if len > MAX_LEN as usize {
                        return Err(PulseError::Oversized);
                    }
                    let blob = self.read_arbitrary()?;
                    if blob.len() != len {
                        return Err(PulseError::Truncated);
                    }
                    let value = std::str::from_utf8(blob).map_err(|_| PulseError::InvalidUtf8)?;
                    pairs.push((key, value.to_owned()));
                }
                TAG_STRING_NULL => {
                    self.take(1)?;
                    break;
                }
                other => return Err(PulseError::InvalidTag(other)),
            }
        }
        Ok(pairs)
    }

    /// Reads a tagged 6-byte sample spec, returning
    /// `(sample_rate, channels, format_byte)` — PA field order on the wire
    /// is format(u8), channels(u8), rate(u32 BE). The format byte is
    /// returned verbatim — the full PA format set is wider than
    /// [`crate::codec::SampleFormat`], so no enum mapping happens here.
    pub fn read_sample_spec(&mut self) -> Result<(u32, u8, u8), PulseError> {
        self.expect_tag(TAG_SAMPLE_SPEC)?;
        let bytes = self.take(6)?;
        let sample_rate = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        Ok((sample_rate, bytes[1], bytes[0]))
    }

    /// Reads a tagged volume (`u32` payload).
    pub fn read_volume(&mut self) -> Result<u32, PulseError> {
        self.expect_tag(TAG_VOLUME)?;
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a tagged cvolume — mirror of the C reader (`get_cvolume`):
    /// `'v'` tag, then a RAW `u8` count and RAW `u32` BE volumes (the
    /// writer is equally raw — no per-value tags on either side).
    pub fn read_cvolume(&mut self) -> Result<Vec<u32>, PulseError> {
        self.expect_tag(TAG_CVOLUME)?;
        let count = self.take(1)?[0] as usize;
        if count > MAX_LEN as usize {
            return Err(PulseError::Oversized);
        }
        let bytes = self.take(count * 4)?;
        Ok((0..count)
            .map(|i| {
                u32::from_be_bytes([
                    bytes[i * 4],
                    bytes[i * 4 + 1],
                    bytes[i * 4 + 2],
                    bytes[i * 4 + 3],
                ])
            })
            .collect())
    }

    /// Reads a tagged channel map, returning the raw position bytes
    /// (`PA_CHANNEL_POSITION_*` values, e.g. `[1, 2]` for stereo).
    pub fn read_channel_map(&mut self) -> Result<Vec<u8>, PulseError> {
        self.expect_tag(TAG_CHANNEL_MAP)?;
        let count = self.read_u8_raw()?;
        Ok(self.take(usize::from(count))?.to_vec())
    }

    /// Skips a **raw** (untagged) channel map: `u8` position count + that
    /// many position bytes.
    pub fn skip_channel_map(&mut self) -> Result<(), PulseError> {
        self.read_channel_map().map(|_| ())
    }

    /// Reads one raw (untagged) byte.
    fn read_u8_raw(&mut self) -> Result<u8, PulseError> {
        Ok(self.take(1)?[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_roundtrip_scalars_strings_and_arbitrary() {
        let mut w = TagWriter::new();
        w.u32(0xDEAD_BEEF)
            .u8(0x7F)
            .boolean(true)
            .boolean(false)
            .string("sonicbrew gw-pulse")
            .string_null()
            .arbitrary(&[0x5A; 256])
            .u64(0x0102_0304_0506_0708);
        let bytes = w.into_bytes();

        let mut r = TagReader::new(&bytes);
        assert_eq!(r.read_u32().expect("u32"), 0xDEAD_BEEF);
        assert_eq!(r.read_u8().expect("u8"), 0x7F);
        assert!(r.read_boolean().expect("bool true"));
        assert!(!r.read_boolean().expect("bool false"));
        assert_eq!(
            r.read_string().expect("string"),
            Some("sonicbrew gw-pulse".to_owned())
        );
        assert_eq!(r.read_string().expect("null string"), None);
        assert_eq!(r.read_arbitrary().expect("arbitrary"), &[0x5A; 256]);
        assert_eq!(r.read_u64().expect("u64"), 0x0102_0304_0506_0708);
        assert!(r.is_empty());
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn tag_proplist_roundtrip() {
        let mut w = TagWriter::new();
        w.proplist(&[
            ("application.name", "sonicbrew"),
            ("application.process.id", "4242"),
        ]);
        let bytes = w.into_bytes();

        let mut r = TagReader::new(&bytes);
        let pairs = r.read_proplist().expect("proplist");
        assert_eq!(
            pairs,
            vec![
                ("application.name".to_owned(), "sonicbrew".to_owned()),
                ("application.process.id".to_owned(), "4242".to_owned()),
            ]
        );
        assert!(r.is_empty());
    }

    #[test]
    fn tag_raw_sample_spec_and_channel_map_roundtrip() {
        let mut w = TagWriter::new();
        w.sample_spec(48_000, 2, 5);
        w.channel_map(&[0, 1]);
        let bytes = w.into_bytes();
        // 'a' + 6-byte spec + 'm' + count + 2 positions.
        assert_eq!(bytes.len(), 1 + 6 + 1 + 1 + 2);

        let mut r = TagReader::new(&bytes);
        assert_eq!(r.read_sample_spec().expect("spec"), (48_000, 2, 5));
        r.skip_channel_map().expect("channel map");
        assert!(r.is_empty());
    }

    #[test]
    fn tag_volume_cvolume_and_channel_map_read_roundtrip() {
        let mut w = TagWriter::new();
        w.volume(0x10000);
        w.cvolume(&[0x10000, 0x8000]);
        w.channel_map(&[1, 2]);
        let bytes = w.into_bytes();

        let mut r = TagReader::new(&bytes);
        assert_eq!(r.read_volume().expect("volume"), 0x10000);
        // cvolume = 'v' tag + RAW u8 count + raw u32 volumes (matches the
        // C put/get asymmetry: the writer uses raw fields, no per-value
        // tags).
        assert_eq!(r.read_cvolume().expect("cvolume"), vec![0x10000, 0x8000]);
        assert_eq!(r.read_channel_map().expect("channel map"), vec![1, 2]);
        assert!(r.is_empty());
    }

    #[test]
    fn tag_reader_rejects_truncated_and_mismatched_tags() {
        // U32 tag byte present but payload cut short.
        let mut r = TagReader::new(&[TAG_U32, 0x00, 0x00]);
        assert!(matches!(r.read_u32(), Err(PulseError::Truncated)));

        // Expected a U32 tag, found a string tag.
        let mut r = TagReader::new(&[TAG_STRING]);
        assert!(matches!(
            r.read_u32(),
            Err(PulseError::InvalidTag(TAG_STRING))
        ));

        // Empty buffer is exhausted, not truncated-at-tag.
        let mut r = TagReader::new(&[]);
        assert!(r.is_empty());
        assert!(matches!(r.read_u32(), Err(PulseError::Truncated)));
    }

    #[test]
    fn tag_reader_rejects_oversized_and_non_utf8_strings() {
        // NUL-terminated string whose length exceeds MAX_LEN.
        let mut buf = vec![TAG_STRING];
        buf.extend(std::iter::repeat_n(b'a', MAX_LEN as usize + 1));
        buf.push(0);
        let mut r = TagReader::new(&buf);
        assert!(matches!(r.read_string(), Err(PulseError::Oversized)));

        // Valid NUL-terminated payload with non-UTF-8 bytes.
        let mut buf = vec![TAG_STRING];
        buf.extend_from_slice(&[0xFF, 0xFE]);
        buf.push(0);
        let mut r = TagReader::new(&buf);
        assert!(matches!(r.read_string(), Err(PulseError::InvalidUtf8)));

        // Unterminated string (no NUL anywhere) is truncated.
        let buf = vec![TAG_STRING, b'a', b'b'];
        let mut r = TagReader::new(&buf);
        assert!(matches!(r.read_string(), Err(PulseError::Truncated)));
    }
}
