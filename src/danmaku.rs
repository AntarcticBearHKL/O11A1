//! Hand-written protobuf decoding for Bilibili danmaku segments.
//!
//! `GET /x/v2/dm/web/seg.so` returns a raw protobuf `DmSegMobileReply`
//! (`application/octet-stream`). No protobuf crate is linked; the wire format is
//! parsed directly and every field we do not care about is skipped, so unknown
//! or newer fields never fail decoding.
//!
//! Wire format rules used here:
//! * a field starts with a varint tag: `field = tag >> 3`, `wire = tag & 7`;
//! * wire 0 = varint, wire 1 = 8 fixed bytes, wire 2 = varint length + bytes,
//!   wire 5 = 4 fixed bytes.

use anyhow::{anyhow, bail, Result};

/// One danmaku comment as exposed to the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Danmaku {
    /// Playback time the comment should appear, in milliseconds.
    pub progress_ms: u32,
    /// 1-3 = scrolling (right to left), 4 = bottom, 5 = top.
    pub mode: u32,
    /// API font size hint (18 / 25 / 36).
    pub fontsize: u32,
    /// Decimal RGB (e.g. `0xFFFFFF` for white).
    pub color: u32,
    /// UTF-8 comment body.
    pub text: String,
    /// Sender id, hashed by Bilibili (`midHash`, field 6). Used for per-sender
    /// blocking; empty when the server omitted it.
    pub mid_hash: String,
    /// Bilibili's own danmaku id (`idStr`, field 12; falls back to the numeric
    /// `id`, field 1). Required by the 举报 / 点赞 endpoints; empty when the
    /// server omitted both.
    pub dmid: String,
}

/// Decode one `DmSegMobileReply` segment into renderable danmaku.
///
/// Only modes 1..=5 are kept and `pool == 2` (subtitle) entries are dropped; the
/// result is sorted by `progress_ms`. Truncated input yields the entries decoded
/// so far instead of an error, so one bad segment cannot abort playback.
pub fn decode_segment(bytes: &[u8]) -> Vec<Danmaku> {
    let mut reader = Reader::new(bytes);
    let mut out = Vec::new();

    while !reader.eof() {
        let Ok(tag) = reader.read_varint() else {
            break;
        };
        let field = (tag >> 3) as u32;
        let wire = (tag & 7) as u8;

        if field == 1 && wire == 2 {
            // DanmakuElem, length-delimited.
            let Ok(len) = reader.read_varint() else {
                break;
            };
            let Ok(chunk) = reader.read_slice(len as usize) else {
                break;
            };
            if let Some(danmaku) = decode_elem(chunk) {
                out.push(danmaku);
            }
        } else if reader.skip(wire).is_err() {
            break;
        }
    }

    out.sort_by_key(|item| item.progress_ms);
    out
}

/// Decode a single `DanmakuElem`. Returns `None` when the entry is a mode we do
/// not render or a subtitle (`pool == 2`) entry.
fn decode_elem(chunk: &[u8]) -> Option<Danmaku> {
    let mut reader = Reader::new(chunk);
    let mut danmaku = Danmaku::default();
    let mut pool: u32 = 0;

    while !reader.eof() {
        let Ok(tag) = reader.read_varint() else {
            break;
        };
        let field = (tag >> 3) as u32;
        let wire = (tag & 7) as u8;

        let handled = match (field, wire) {
            (1, 0) => reader
                .read_varint()
                .map(|value| {
                    if danmaku.dmid.is_empty() {
                        danmaku.dmid = value.to_string();
                    }
                })
                .is_ok(),
            (2, 0) => reader
                .read_varint()
                .map(|value| danmaku.progress_ms = value as u32)
                .is_ok(),
            (3, 0) => reader
                .read_varint()
                .map(|value| danmaku.mode = value as u32)
                .is_ok(),
            (4, 0) => reader
                .read_varint()
                .map(|value| danmaku.fontsize = value as u32)
                .is_ok(),
            (5, 0) => reader
                .read_varint()
                .map(|value| danmaku.color = value as u32)
                .is_ok(),
            (6, 2) => match reader.read_varint() {
                Ok(len) => match reader.read_slice(len as usize) {
                    Ok(hash) => {
                        danmaku.mid_hash = String::from_utf8_lossy(hash).into_owned();
                        true
                    }
                    Err(_) => false,
                },
                Err(_) => false,
            },
            (11, 0) => reader
                .read_varint()
                .map(|value| pool = value as u32)
                .is_ok(),
            (12, 2) => match reader.read_varint() {
                Ok(len) => match reader.read_slice(len as usize) {
                    Ok(id) => {
                        let text = String::from_utf8_lossy(id).into_owned();
                        if !text.is_empty() {
                            danmaku.dmid = text;
                        }
                        true
                    }
                    Err(_) => false,
                },
                Err(_) => false,
            },
            (7, 2) => match reader.read_varint() {
                Ok(len) => match reader.read_slice(len as usize) {
                    Ok(text) => {
                        danmaku.text = String::from_utf8_lossy(text).into_owned();
                        true
                    }
                    Err(_) => false,
                },
                Err(_) => false,
            },
            _ => reader.skip(wire).is_ok(),
        };

        if !handled {
            break;
        }
    }

    if !(1..=5).contains(&danmaku.mode) {
        return None;
    }
    if pool == 2 {
        return None;
    }
    Some(danmaku)
}

/// Parse the historical-danmaku XML from `x/v2/dm/history`.
///
/// Each comment is `<d p="progress_ms,mode,fontsize,color,ctime,pool,midHash,dmid">text</d>`.
/// Unknown elements/attributes are ignored and malformed entries skipped. Only
/// modes 1..=5 are kept and `pool == 2` (subtitle) entries are dropped, matching
/// [`decode_segment`]; the result is sorted by `progress_ms`.
pub fn parse_history_xml(xml: &str) -> Vec<Danmaku> {
    let mut out = Vec::new();
    let mut cursor = 0usize;

    while cursor < xml.len() {
        let Some(open_rel) = xml[cursor..].find("<d ") else {
            break;
        };
        let open = cursor + open_rel;
        let Some(gt_rel) = xml[open..].find('>') else {
            break;
        };
        let gt = open + gt_rel;
        let Some(close_rel) = xml[gt + 1..].find("</d>") else {
            break;
        };
        let close = gt + 1 + close_rel;
        let text = decode_xml_entities(&xml[gt + 1..close]);

        if let Some(fields) = attr_value(&xml[open..gt], "p") {
            let parts: Vec<&str> = fields.split(',').collect();
            if let Some(danmaku) = danmaku_from_history(&parts, text) {
                out.push(danmaku);
            }
        }

        cursor = close + "</d>".len();
    }

    out.sort_by_key(|item| item.progress_ms);
    out
}

/// Build one [`Danmaku`] from a split `p` attribute, or `None` for a mode/pool
/// this app does not render.
fn danmaku_from_history(parts: &[&str], text: String) -> Option<Danmaku> {
    if parts.len() < 7 {
        return None;
    }
    let parse = |index: usize, fallback: u32| {
        parts
            .get(index)
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(fallback)
    };
    let mode = parse(1, 1);
    let pool = parse(5, 0);
    if !(1..=5).contains(&mode) || pool == 2 {
        return None;
    }
    Some(Danmaku {
        progress_ms: parse(0, 0),
        mode,
        fontsize: parse(2, 25),
        color: parse(3, 0x00FF_FFFF),
        text,
        mid_hash: parts.get(6).map(|v| v.trim().to_string()).unwrap_or_default(),
        dmid: parts.get(7).map(|v| v.trim().to_string()).unwrap_or_default(),
    })
}

/// Value of `name="..."` inside an already-isolated start tag.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Decode the five predefined XML entities plus `&#NNN;` / `&#xHH;`.
fn decode_xml_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let after = &rest[index..];
        if let Some(end) = after.find(';') {
            let entity = &after[1..end];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity.strip_prefix('#').and_then(decode_numeric_entity),
            };
            if let Some(ch) = decoded {
                out.push(ch);
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

fn decode_numeric_entity(number: &str) -> Option<char> {
    let code = match number.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => number.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

/// Bounds-checked cursor over a protobuf byte slice.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn read_varint(&mut self) -> Result<u64> {
        let mut value: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let Some(&byte) = self.bytes.get(self.pos) else {
                bail!("danmaku: truncated varint");
            };
            self.pos += 1;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= 64 {
                bail!("danmaku: varint overflow");
            }
        }
    }

    fn read_slice(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| anyhow!("danmaku: length overflow"))?;
        if end > self.bytes.len() {
            bail!("danmaku: truncated length-delimited field");
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Advance past one field whose value we ignore.
    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.read_varint()?;
            }
            1 => {
                self.read_slice(8)?;
            }
            2 => {
                let len = self.read_varint()? as usize;
                self.read_slice(len)?;
            }
            5 => {
                self.read_slice(4)?;
            }
            _ => bail!("danmaku: unsupported wire type {wire}"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn write_tag(field: u32, wire: u8, out: &mut Vec<u8>) {
        write_varint(((field << 3) | wire as u32) as u64, out);
    }

    fn write_varint_field(field: u32, value: u64, out: &mut Vec<u8>) {
        write_tag(field, 0, out);
        write_varint(value, out);
    }

    fn write_string_field(field: u32, text: &str, out: &mut Vec<u8>) {
        write_tag(field, 2, out);
        write_varint(text.len() as u64, out);
        out.extend_from_slice(text.as_bytes());
    }

    /// Build one `DanmakuElem` payload.
    fn build_elem(
        progress: u64,
        mode: u64,
        fontsize: u64,
        color: u64,
        content: &str,
        pool: u64,
    ) -> Vec<u8> {
        build_elem_hash(progress, mode, fontsize, color, content, pool, "")
    }

    fn build_elem_hash(
        progress: u64,
        mode: u64,
        fontsize: u64,
        color: u64,
        content: &str,
        pool: u64,
        mid_hash: &str,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint_field(2, progress, &mut buf);
        write_varint_field(3, mode, &mut buf);
        write_varint_field(4, fontsize, &mut buf);
        write_varint_field(5, color, &mut buf);
        if !mid_hash.is_empty() {
            write_string_field(6, mid_hash, &mut buf);
        }
        write_string_field(7, content, &mut buf);
        write_varint_field(11, pool, &mut buf);
        buf
    }

    fn push_elem(reply: &mut Vec<u8>, elem: &[u8]) {
        write_tag(1, 2, reply);
        write_varint(elem.len() as u64, reply);
        reply.extend_from_slice(elem);
    }

    #[test]
    fn decodes_two_elements_and_skips_subtitles() {
        let first = build_elem(1500, 1, 25, 0x00FF_FFFF, "hello", 0);
        let second = build_elem(2000, 5, 36, 0x00FF_0000, "顶部", 0);
        let subtitle = build_elem(2500, 1, 25, 0x00FF_FFFF, "subtitle", 2);

        let mut reply = Vec::new();
        push_elem(&mut reply, &first);
        push_elem(&mut reply, &second);
        push_elem(&mut reply, &subtitle);

        let decoded = decode_segment(&reply);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].progress_ms, 1500);
        assert_eq!(decoded[0].mode, 1);
        assert_eq!(decoded[0].fontsize, 25);
        assert_eq!(decoded[0].color, 0x00FF_FFFF);
        assert_eq!(decoded[0].text, "hello");
        assert_eq!(decoded[1].progress_ms, 2000);
        assert_eq!(decoded[1].mode, 5);
        assert_eq!(decoded[1].color, 0x00FF_0000);
        assert_eq!(decoded[1].text, "顶部");
    }

    #[test]
    fn decodes_mid_hash_sender() {
        let elem = build_elem_hash(1000, 1, 25, 0x00FF_FFFF, "hi", 0, "7f3a9c");
        let mut reply = Vec::new();
        push_elem(&mut reply, &elem);

        let decoded = decode_segment(&reply);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].mid_hash, "7f3a9c");
        assert_eq!(decoded[0].text, "hi");
    }

    #[test]
    fn truncated_input_does_not_panic() {
        // A tag that promises a length-delimited field but no payload follows.
        let bytes = [0x0a, 0x10, 0x01, 0x02];
        let decoded = decode_segment(&bytes);
        assert!(decoded.is_empty());
    }

    #[test]
    fn decodes_dmid_from_id_str() {
        let mut buf = Vec::new();
        write_varint_field(2, 1000, &mut buf);
        write_varint_field(3, 1, &mut buf);
        write_string_field(12, "1234567890", &mut buf);
        write_string_field(7, "hi", &mut buf);
        let mut reply = Vec::new();
        push_elem(&mut reply, &buf);

        let decoded = decode_segment(&reply);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].dmid, "1234567890");
    }

    #[test]
    fn decodes_dmid_from_numeric_id_when_id_str_absent() {
        let mut buf = Vec::new();
        write_varint_field(1, 42, &mut buf);
        write_varint_field(2, 1000, &mut buf);
        write_varint_field(3, 1, &mut buf);
        write_string_field(7, "hi", &mut buf);
        let mut reply = Vec::new();
        push_elem(&mut reply, &buf);

        let decoded = decode_segment(&reply);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].dmid, "42");
    }

    #[test]
    fn parses_history_xml_fields_and_entities() {
        let xml = r#"<?xml version="1.0"?><i><chatserver>chat.bilibili.com</chatserver>
<d p="1500,1,25,16777215,1700000000,0,abc123,99887766">hello &amp; world</d>
<d p="5200,5,36,16711680,1700000001,0,def456,99887767">顶部弹幕</d>
<d p="9000,1,25,16777215,1700000002,2,sub,99887768">subtitle</d>
<d p="12000,4,18,16777215,1700000003,0,ghi789,99887769">bottom</d>
</i>"#;
        let decoded = parse_history_xml(xml);
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].progress_ms, 1500);
        assert_eq!(decoded[0].mode, 1);
        assert_eq!(decoded[0].text, "hello & world");
        assert_eq!(decoded[0].mid_hash, "abc123");
        assert_eq!(decoded[0].dmid, "99887766");
        assert_eq!(decoded[1].progress_ms, 5200);
        assert_eq!(decoded[1].mode, 5);
        assert_eq!(decoded[1].fontsize, 36);
        assert_eq!(decoded[1].color, 0x00FF_0000);
        assert_eq!(decoded[2].progress_ms, 12000);
        assert_eq!(decoded[2].mode, 4);
        assert!(!decoded.iter().any(|item| item.text == "subtitle"));
    }

    #[test]
    fn history_entry_with_too_few_fields_is_skipped() {
        let xml = r#"<d p="1500,1,25">short</d>"#;
        assert!(parse_history_xml(xml).is_empty());
    }

    #[test]
    fn parses_history_numeric_entity() {
        let xml = r#"<d p="1,1,25,16777215,0,0,h,7">&#128512;&#x41;</d>"#;
        let decoded = parse_history_xml(xml);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].text, "\u{1F600}A");
    }

    #[test]
    fn empty_history_xml_yields_no_entries() {
        assert!(parse_history_xml("<i></i>").is_empty());
        assert!(parse_history_xml("").is_empty());
    }
}
