//! FITS header parsing, kept deliberately small.
//!
//! Only enough of the format to answer two questions without touching pixel
//! data: *what is in this HDU*, and *where does the next one start*. Values
//! land in a `serde_json::Map` keyed by the keyword in upper case, which is
//! exactly the shape `wcs::WCSParams` deserialises from, so a WCS can be built
//! from a header we parsed ourselves.

use crate::error::{Error, Result};
use serde_json::{Map, Number, Value};

/// FITS is organised in fixed 2880-byte blocks, headers and data alike.
pub const BLOCK_LEN: usize = 2880;
/// Each header block holds 36 cards of 80 bytes.
pub const CARD_LEN: usize = 80;

pub type Keywords = Map<String, Value>;

/// Round a byte count up to a whole number of FITS blocks.
pub fn padded_to_block(len: u64) -> u64 {
    len.div_ceil(BLOCK_LEN as u64) * (BLOCK_LEN as u64)
}

/// Split a card's value field from its trailing comment.
///
/// The `/` that starts a comment does not count while inside a quoted string,
/// and `''` inside such a string is an escaped quote, not the end of it.
fn split_value_and_comment(field: &str) -> &str {
    let bytes = field.as_bytes();
    let mut in_string = false;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                if in_string && bytes.get(i + 1) == Some(&b'\'') {
                    i += 1; // escaped quote, stay inside the string
                } else {
                    in_string = !in_string;
                }
            }
            b'/' if !in_string => return &field[..i],
            _ => {}
        }
        i += 1;
    }

    field
}

fn parse_value(raw: &str) -> Value {
    let text = raw.trim();

    if text.is_empty() {
        return Value::Null;
    }

    if let Some(rest) = text.strip_prefix('\'') {
        // Strings run to the closing quote; '' is a literal quote. Trailing
        // spaces are not significant in FITS.
        let mut out = String::new();
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                } else {
                    break;
                }
            } else {
                out.push(c);
            }
        }
        return Value::String(out.trim_end().to_string());
    }

    if text == "T" {
        return Value::Bool(true);
    }
    if text == "F" {
        return Value::Bool(false);
    }

    if let Ok(i) = text.parse::<i64>() {
        return Value::Number(i.into());
    }

    // FITS writes double-precision exponents with D, which Rust will not parse.
    let normalised = text.replace(['D', 'd'], "E");
    if let Ok(f) = normalised.parse::<f64>() {
        if let Some(n) = Number::from_f64(f) {
            return Value::Number(n);
        }
    }

    // Complex values and anything else we do not model are kept verbatim
    // rather than dropped, so nothing silently disappears from the header.
    Value::String(text.to_string())
}

/// Parse one 80-byte card into a `(key, value)` pair.
///
/// Returns `None` for cards that carry no value: `END`, `COMMENT`, `HISTORY`,
/// blank cards, and `CONTINUE` (long-string continuations, which nothing in
/// this pipeline needs).
fn parse_card(card: &[u8]) -> Option<(String, Value)> {
    let text = String::from_utf8_lossy(card);
    let key = text.get(..8)?.trim().to_string();

    if key.is_empty() || key == "END" || key == "COMMENT" || key == "HISTORY" || key == "CONTINUE" {
        return None;
    }

    // A value is present only when the card has "= " in bytes 8 and 9.
    if text.get(8..10) != Some("= ") {
        return None;
    }

    let field = text.get(10..)?;
    // Upper case is both the FITS convention and what WCSParams expects, so
    // the header map can be handed to it without translation.
    Some((key.to_uppercase(), parse_value(split_value_and_comment(field))))
}

/// One parsed 2880-byte header block.
pub struct ParsedBlock {
    pub keywords: Keywords,
    /// Whether this block contained the `END` card that closes the header.
    pub end: bool,
}

/// Parse a single header block. `bytes` must be exactly [`BLOCK_LEN`] long.
pub fn parse_block(bytes: &[u8]) -> Result<ParsedBlock> {
    if bytes.len() != BLOCK_LEN {
        return Err(Error::Format(format!(
            "header block is {} bytes, expected {}",
            bytes.len(),
            BLOCK_LEN
        )));
    }

    let mut keywords = Keywords::new();
    let mut end = false;

    for card in bytes.chunks_exact(CARD_LEN) {
        // The END card is the keyword followed by spaces for the rest of the
        // 80 bytes. Requiring the trailing space keeps ENDIAN and friends from
        // terminating the header early.
        if card.starts_with(b"END ") {
            end = true;
            break;
        }
        if let Some((key, value)) = parse_card(card) {
            keywords.insert(key, value);
        }
    }

    Ok(ParsedBlock { keywords, end })
}

/// Typed reads over a parsed header.
pub trait KeywordsExt {
    fn int(&self, key: &str) -> Option<i64>;
    fn float(&self, key: &str) -> Option<f64>;
    fn text(&self, key: &str) -> Option<&str>;
}

impl KeywordsExt for Keywords {
    fn int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            _ => None,
        }
    }

    fn float(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Value::Number(n) => n.as_f64(),
            _ => None,
        }
    }

    fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// The axis lengths, `NAXIS1` first.
pub fn axes(keywords: &Keywords) -> Vec<u64> {
    let naxis = keywords.int("NAXIS").unwrap_or(0).max(0) as usize;
    (1..=naxis)
        .map(|i| keywords.int(&format!("NAXIS{}", i)).unwrap_or(0).max(0) as u64)
        .collect()
}

/// Size of this HDU's data unit in bytes, before padding to a block boundary.
///
/// The general form covers random groups and binary tables as well as plain
/// images: `|BITPIX|/8 * GCOUNT * (PCOUNT + NAXIS1 * ... * NAXISn)`.
pub fn data_unit_len(keywords: &Keywords) -> u64 {
    let axes = axes(keywords);
    if axes.is_empty() || axes.iter().any(|&n| n == 0) {
        return 0;
    }

    let bitpix = keywords.int("BITPIX").unwrap_or(0).unsigned_abs();
    let bytes_per_value = bitpix / 8;
    let pcount = keywords.int("PCOUNT").unwrap_or(0).max(0) as u64;
    let gcount = keywords.int("GCOUNT").unwrap_or(1).max(0) as u64;

    let elements: u64 = axes.iter().product();

    bytes_per_value
        .saturating_mul(gcount)
        .saturating_mul(pcount.saturating_add(elements))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(text: &str) -> Vec<u8> {
        let mut c = text.as_bytes().to_vec();
        c.resize(CARD_LEN, b' ');
        c
    }

    #[test]
    fn parses_the_value_types_that_matter() {
        assert_eq!(parse_card(&card("SIMPLE  =                    T")).unwrap(),
                   ("SIMPLE".into(), Value::Bool(true)));
        assert_eq!(parse_card(&card("BITPIX  =                  -32")).unwrap(),
                   ("BITPIX".into(), Value::Number((-32).into())));
        assert_eq!(parse_card(&card("CTYPE1  = 'RA---TAN'")).unwrap(),
                   ("CTYPE1".into(), Value::String("RA---TAN".into())));
        assert_eq!(parse_card(&card("CRVAL1  =    8.3822000000E+01")).unwrap().1,
                   Value::Number(Number::from_f64(83.822).unwrap()));
    }

    #[test]
    fn handles_fortran_double_exponents() {
        // Written by older FITS writers; Rust's float parser rejects the D.
        assert_eq!(
            parse_card(&card("CDELT1  =  -1.234D-04")).unwrap().1,
            Value::Number(Number::from_f64(-1.234e-4).unwrap())
        );
    }

    #[test]
    fn a_slash_inside_a_string_is_not_a_comment() {
        let (_, value) = parse_card(&card("FILE    = 'a/b/c.fits' / where it came from")).unwrap();
        assert_eq!(value, Value::String("a/b/c.fits".into()));
    }

    #[test]
    fn doubled_quotes_are_escaped_quotes() {
        let (_, value) = parse_card(&card("OBJECT  = 'Barnard''s star'")).unwrap();
        assert_eq!(value, Value::String("Barnard's star".into()));
    }

    #[test]
    fn valueless_cards_are_skipped() {
        assert!(parse_card(&card("COMMENT   nothing to see")).is_none());
        assert!(parse_card(&card("HISTORY   processed")).is_none());
        assert!(parse_card(&card("END")).is_none());
        assert!(parse_card(&card("")).is_none());
        // No "= " in columns 9-10 means no value, whatever follows.
        assert!(parse_card(&card("HIERARCH ESO DET ID = 'x'")).is_none());
    }

    #[test]
    fn finds_the_end_card() {
        let mut block = Vec::new();
        block.extend(card("SIMPLE  =                    T"));
        block.extend(card("BITPIX  =                  -32"));
        block.extend(card("END"));
        block.resize(BLOCK_LEN, b' ');

        let parsed = parse_block(&block).unwrap();
        assert!(parsed.end);
        assert_eq!(parsed.keywords.int("BITPIX"), Some(-32));
    }

    fn image_keywords(bitpix: i64, axes: &[u64]) -> Keywords {
        let mut k = Keywords::new();
        k.insert("BITPIX".into(), Value::Number(bitpix.into()));
        k.insert("NAXIS".into(), Value::Number((axes.len() as i64).into()));
        for (i, n) in axes.iter().enumerate() {
            k.insert(format!("NAXIS{}", i + 1), Value::Number((*n as i64).into()));
        }
        k
    }

    #[test]
    fn computes_data_unit_size() {
        // 4096 x 4096 float32
        assert_eq!(
            data_unit_len(&image_keywords(-32, &[4096, 4096])),
            4096 * 4096 * 4
        );
        // NAXIS = 0 means no data unit at all.
        assert_eq!(data_unit_len(&image_keywords(8, &[])), 0);
        // A zero-length axis also means no data.
        assert_eq!(data_unit_len(&image_keywords(16, &[0, 10])), 0);
    }

    #[test]
    fn counts_binary_table_heap_and_groups() {
        let mut k = image_keywords(8, &[100, 5]);
        k.insert("PCOUNT".into(), Value::Number(320.into()));
        k.insert("GCOUNT".into(), Value::Number(1.into()));
        assert_eq!(data_unit_len(&k), 100 * 5 + 320);
    }

    #[test]
    fn pads_to_block_boundaries() {
        assert_eq!(padded_to_block(0), 0);
        assert_eq!(padded_to_block(1), 2880);
        assert_eq!(padded_to_block(2880), 2880);
        assert_eq!(padded_to_block(2881), 5760);
    }
}
