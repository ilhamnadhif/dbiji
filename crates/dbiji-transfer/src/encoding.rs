//! Streaming character set conversion with `encoding_rs`.
//!
//! Sinks always produce UTF-8 text; [`EncodingWriter`] converts it to the
//! requested output encoding on the fly. Readers convert any input encoding to
//! UTF-8 with [`DecodingReader`] before parsing.

use dbiji_core::{DbError, DbResult};
use encoding_rs::{CoderResult, Decoder, Encoder, EncoderResult, Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};
use std::io::{self, Read, Write};

/// Look up an encoding by its WHATWG label (`UTF-8`, `utf-16le`, `windows-1252`,
/// `latin1`, `shift_jis` ...). An empty label means UTF-8.
pub(crate) fn resolve_encoding(label: &str) -> DbResult<&'static Encoding> {
    let label = label.trim();
    if label.is_empty() {
        return Ok(UTF_8);
    }
    Encoding::for_label(label.as_bytes())
        .ok_or_else(|| DbError::config(format!("Unknown character encoding '{label}'")))
}

/// Byte order mark for encodings that have one.
pub(crate) fn bom_bytes(encoding: &'static Encoding) -> &'static [u8] {
    if encoding == UTF_8 {
        &[0xEF, 0xBB, 0xBF]
    } else if encoding == UTF_16LE {
        &[0xFF, 0xFE]
    } else if encoding == UTF_16BE {
        &[0xFE, 0xFF]
    } else {
        &[]
    }
}

enum Mode {
    Utf8,
    Utf16 { little_endian: bool },
    Legacy(Box<Encoder>),
}

/// `Write` adapter converting UTF-8 input into the target encoding.
/// Characters that cannot be represented are replaced with `?`.
pub(crate) struct EncodingWriter<W: Write> {
    inner: W,
    mode: Mode,
    /// Incomplete UTF-8 sequence left over from the previous write.
    pending: Vec<u8>,
    scratch: Vec<u8>,
    unmappable: u64,
}

impl<W: Write> EncodingWriter<W> {
    pub(crate) fn new(inner: W, encoding: &'static Encoding) -> Self {
        let mode = if encoding == UTF_8 {
            Mode::Utf8
        } else if encoding == UTF_16LE {
            Mode::Utf16 { little_endian: true }
        } else if encoding == UTF_16BE {
            Mode::Utf16 { little_endian: false }
        } else {
            Mode::Legacy(Box::new(encoding.new_encoder()))
        };
        EncodingWriter { inner, mode, pending: Vec::new(), scratch: Vec::new(), unmappable: 0 }
    }

    /// Number of characters replaced because the encoding cannot represent them.
    pub(crate) fn unmappable(&self) -> u64 {
        self.unmappable
    }

    fn encode_str(&mut self, s: &str, last: bool) -> io::Result<()> {
        match &mut self.mode {
            Mode::Utf8 => self.inner.write_all(s.as_bytes()),
            Mode::Utf16 { little_endian } => {
                self.scratch.clear();
                for unit in s.encode_utf16() {
                    let b = if *little_endian { unit.to_le_bytes() } else { unit.to_be_bytes() };
                    self.scratch.extend_from_slice(&b);
                }
                self.inner.write_all(&self.scratch)
            }
            Mode::Legacy(encoder) => {
                let mut rest = s;
                loop {
                    let cap = encoder
                        .max_buffer_length_from_utf8_without_replacement(rest.len())
                        .unwrap_or(rest.len().saturating_mul(4))
                        .clamp(16, 64 * 1024);
                    self.scratch.resize(cap, 0);
                    let (result, read, written) =
                        encoder.encode_from_utf8_without_replacement(rest, &mut self.scratch, last);
                    self.inner.write_all(&self.scratch[..written])?;
                    rest = &rest[read..];
                    match result {
                        EncoderResult::InputEmpty => return Ok(()),
                        EncoderResult::OutputFull => continue,
                        EncoderResult::Unmappable(_) => {
                            self.unmappable += 1;
                            self.inner.write_all(b"?")?;
                        }
                    }
                }
            }
        }
    }

    /// Flush the encoder state; must be called once after the last write.
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.pending.clear();
            return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete UTF-8 sequence at end of output"));
        }
        if matches!(self.mode, Mode::Legacy(_)) {
            self.encode_str("", true)?;
        }
        self.inner.flush()
    }

    /// The wrapped writer.
    pub(crate) fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

impl<W: Write> Write for EncodingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if matches!(self.mode, Mode::Utf8) && self.pending.is_empty() {
            self.inner.write_all(buf)?;
            return Ok(buf.len());
        }
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(buf);
        let valid = match std::str::from_utf8(&data) {
            Ok(s) => s.len(),
            // Incomplete sequence at the end: keep it for the next write.
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "output text is not valid UTF-8")),
        };
        let text = std::str::from_utf8(&data[..valid]).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.encode_str(text, false)?;
        data.drain(..valid);
        self.pending = data;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// `Read` adapter converting the input encoding into UTF-8.
///
/// A byte order mark always wins over the configured encoding and is removed.
/// Malformed input is replaced with U+FFFD and reported by [`had_errors`](Self::had_errors).
pub(crate) struct DecodingReader<R: Read> {
    inner: R,
    decoder: Decoder,
    encoding: &'static Encoding,
    raw: Vec<u8>,
    raw_pos: usize,
    raw_len: usize,
    out: Vec<u8>,
    out_pos: usize,
    out_len: usize,
    eof: bool,
    finished: bool,
    had_errors: bool,
}

const RAW_CAPACITY: usize = 64 * 1024;

impl<R: Read> DecodingReader<R> {
    /// Create a decoding reader. `label` is an encoding label, or `auto` / empty
    /// to detect: BOM first, then UTF-8 if the beginning of the input is valid
    /// UTF-8, windows-1252 otherwise.
    pub(crate) fn new(mut inner: R, label: &str) -> DbResult<Self> {
        let mut raw = vec![0u8; RAW_CAPACITY];
        let mut raw_len = 0usize;
        let mut eof = false;
        // Read ahead so BOM sniffing and auto detection see enough bytes.
        while raw_len < raw.len() {
            match inner.read(&mut raw[raw_len..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => raw_len += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(DbError::io(format!("Cannot read input: {e}"))),
            }
        }
        let head = &raw[..raw_len];
        let label = label.trim();
        let encoding = if let Some((enc, _)) = Encoding::for_bom(head) {
            enc
        } else if label.is_empty() || label.eq_ignore_ascii_case("auto") {
            match std::str::from_utf8(head) {
                Ok(_) => UTF_8,
                // Only an incomplete sequence cut by the read-ahead window.
                Err(e) if e.error_len().is_none() && !eof => UTF_8,
                Err(_) => WINDOWS_1252,
            }
        } else {
            resolve_encoding(label)?
        };
        Ok(DecodingReader {
            inner,
            decoder: encoding.new_decoder(),
            encoding,
            raw,
            raw_pos: 0,
            raw_len,
            out: vec![0u8; RAW_CAPACITY * 3 + 16],
            out_pos: 0,
            out_len: 0,
            eof,
            finished: false,
            had_errors: false,
        })
    }

    /// Encoding actually used (after BOM sniffing / detection).
    pub(crate) fn encoding(&self) -> &'static Encoding {
        self.encoding
    }

    /// Malformed byte sequences were replaced with U+FFFD.
    pub(crate) fn had_errors(&self) -> bool {
        self.had_errors
    }
}

impl<R: Read> Read for DecodingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.out_pos < self.out_len {
                let n = buf.len().min(self.out_len - self.out_pos);
                buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            if self.finished || buf.is_empty() {
                return Ok(0);
            }
            if self.raw_pos == self.raw_len && !self.eof {
                let n = loop {
                    match self.inner.read(&mut self.raw) {
                        Ok(n) => break n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                };
                self.raw_pos = 0;
                self.raw_len = n;
                if n == 0 {
                    self.eof = true;
                }
            }
            let last = self.eof && self.raw_pos == self.raw_len;
            let (result, read, written, errors) =
                self.decoder.decode_to_utf8(&self.raw[self.raw_pos..self.raw_len], &mut self.out, last);
            self.raw_pos += read;
            self.out_pos = 0;
            self.out_len = written;
            self.had_errors |= errors;
            if last && result == CoderResult::InputEmpty {
                self.finished = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(label: &str, text: &str, chunk: usize) -> Vec<u8> {
        let enc = resolve_encoding(label).unwrap();
        let mut w = EncodingWriter::new(Vec::new(), enc);
        for part in text.as_bytes().chunks(chunk) {
            w.write_all(part).unwrap();
        }
        w.finish().unwrap();
        w.inner
    }

    fn decode(label: &str, bytes: &[u8]) -> String {
        let mut r = DecodingReader::new(bytes, label).unwrap();
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn utf16_roundtrip_with_split_sequences() {
        let text = "héllo 😀 wörld\n";
        for chunk in [1, 2, 3, 5, 64] {
            let le = encode("UTF-16LE", text, chunk);
            assert_eq!(&le[..2], &[b'h', 0]);
            assert_eq!(decode("utf-16le", &le), text);
            let be = encode("UTF-16BE", text, chunk);
            assert_eq!(&be[..2], &[0, b'h']);
            assert_eq!(decode("utf-16be", &be), text);
        }
    }

    #[test]
    fn windows_1252_with_unmappable() {
        let enc = resolve_encoding("windows-1252").unwrap();
        let mut w = EncodingWriter::new(Vec::new(), enc);
        w.write_all("café €5 😀".as_bytes()).unwrap();
        w.finish().unwrap();
        assert_eq!(w.unmappable(), 1);
        assert_eq!(w.inner, b"caf\xe9 \x805 ?".to_vec());
        assert_eq!(decode("windows-1252", &w.inner), "café €5 ?");
    }

    #[test]
    fn bom_wins_and_is_removed() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("ünï".as_bytes());
        assert_eq!(decode("windows-1252", &bytes), "ünï");
        let mut le = vec![0xFF, 0xFE];
        le.extend(encode("utf-16le", "a,b", 4));
        let r = DecodingReader::new(&le[..], "auto").unwrap();
        assert_eq!(r.encoding(), UTF_16LE);
        assert_eq!(decode("auto", &le), "a,b");
    }

    #[test]
    fn auto_detects_legacy() {
        assert_eq!(decode("auto", b"caf\xe9"), "café");
        assert_eq!(decode("", "café".as_bytes()), "café");
        assert!(resolve_encoding("no-such-encoding").is_err());
    }

    #[test]
    fn large_input_streams() {
        let text: String = (0..50_000).map(|i| format!("línea {i}\n")).collect();
        let bytes = encode("ISO-8859-1", &text, 7_919);
        assert_eq!(decode("latin1", &bytes), text);
    }
}
