//! TNEF (`winmail.dat`) decoding (OUTLOOK_PARITY.md §4.6, item 1.21, M-45).
//!
//! Outlook sends rich mail to external recipients as a small `text/plain`
//! part plus an `application/ms-tnef` part named `winmail.dat`: a binary
//! stream ([MS-OXTNEF]) carrying the real attachments, the rich body (RTF,
//! compressed per [MS-OXRTFCP]) and the message's MAPI properties
//! ([MS-OXCDATA] property stream) - including, on Microsoft 365 tenants, the
//! finding that everything rich about the message otherwise lives behind a
//! MAPI boundary IMAP cannot see.
//!
//! This module is the decoder for that format: the attribute stream, the MAPI
//! property stream (enough of it for the bodies, the message class and the
//! attachment names/inline Content-IDs), the [MS-OXRTFCP] decompressor, and
//! the [MS-OXRTFEX] HTML de-encapsulator. It is pure - no I/O - and lives in
//! `interop` because every name here is a Microsoft wire detail.
//!
//! Callers get a [`TnefContents`] whose attachments are ready to become
//! ordinary `BodyPart`s. The synthetic part-number scheme those parts carry
//! (`tnef:<source part>:<index>`) is defined here too, so the mail session can
//! recognise and answer a `FetchAttachment` for one without a server section
//! fetch.

use std::error::Error as StdError;
/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::fmt;

/// The MIME type Outlook labels a TNEF payload with. `application/x-tnef`
/// and the historical `application/vnd.ms-tnef` are tolerated too: senders
/// and gateways do emit them.
pub const TNEF_CONTENT_TYPE: &str = "application/ms-tnef";

/// The window TNEF's LZ77 dictionary uses, in bytes ([MS-OXRTFCP]).
const RTF_DICT_SIZE: usize = 4096;
/// The preloaded dictionary every compressed RTF stream starts from, copied
/// from [MS-OXRTFCP] §2.4.1; the rest of the window is spaces.
const RTF_INIT_DICT: &[u8] = b"{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";

/// The TNEF stream signature (`[MS-OXTNEF]` §2.1).
const SIGNATURE: u32 = 0x223E_9F78;

// TNEF attribute names: the low 16 bits of the on-wire name field. The high
// 16 bits are a type hint that varies between senders, so only the low word
// is matched.
const ATT_SUBJECT: u16 = 0x8004;
const ATT_MESSAGE_CLASS: u16 = 0x8008;
const ATT_BODY: u16 = 0x800C;
const ATT_ATTACH_DATA: u16 = 0x800F;
const ATT_ATTACH_TITLE: u16 = 0x8010;
const ATT_ATTACH_META_FILE: u16 = 0x8011;
const ATT_ATTACH_TRANSPORT_FILENAME: u16 = 0x9001;
const ATT_ATTACH_RENDDATA: u16 = 0x9002;
const ATT_MAPI_PROPS: u16 = 0x9003;
const ATT_ATTACHMENT: u16 = 0x9005;
const ATT_TNEF_VERSION: u16 = 0x9006;
const ATT_OEM_CODEPAGE: u16 = 0x9007;
const ATT_RTF_COMPRESSED: u16 = 0x9009;

// MAPI property IDs this decoder reads out of an `attMAPIProps` value (or an
// attachment's terminating `attAttachment` value), from [MS-OXPROPS].
const MAPI_MESSAGE_CLASS: u16 = 0x001A;
const MAPI_BODY: u16 = 0x1000;
const MAPI_RTF_COMPRESSED: u16 = 0x1009;
const MAPI_BODY_HTML: u16 = 0x1013;
const MAPI_ATTACH_FILENAME: u16 = 0x3704;
const MAPI_ATTACH_LONG_FILENAME: u16 = 0x3707;
const MAPI_ATTACH_MIME_TAG: u16 = 0x370E;
const MAPI_ATTACH_CONTENT_ID: u16 = 0x3712;
const MAPI_INTERNET_CODEPAGE: u16 = 0x3FDE;

// MAPI property types ([MS-OXCDATA] §2.11.1).
const MAPI_TYPE_SHORT: u16 = 0x0002;
const MAPI_TYPE_INT: u16 = 0x0003;
const MAPI_TYPE_FLOAT: u16 = 0x0004;
const MAPI_TYPE_DOUBLE: u16 = 0x0005;
const MAPI_TYPE_CURRENCY: u16 = 0x0006;
const MAPI_TYPE_APPTIME: u16 = 0x0007;
const MAPI_TYPE_ERROR: u16 = 0x000A;
const MAPI_TYPE_BOOLEAN: u16 = 0x000B;
const MAPI_TYPE_OBJECT: u16 = 0x000D;
const MAPI_TYPE_INT8BYTE: u16 = 0x0014;
const MAPI_TYPE_STRING: u16 = 0x001E;
const MAPI_TYPE_UNICODE_STRING: u16 = 0x001F;
const MAPI_TYPE_SYSTIME: u16 = 0x0040;
const MAPI_TYPE_CLSID: u16 = 0x0048;
const MAPI_TYPE_BINARY: u16 = 0x0102;
const MAPI_TYPE_UNSPECIFIED: u16 = 0x0000;
/// The multi-value bit (`[MS-OXCDATA]` §2.11.1): one property tag followed by
/// a count and that many values.
const MAPI_TYPE_MULTI_VALUE: u16 = 0x1000;
/// Property IDs at or above this are named (GUID) properties, which carry a
/// GUID and a name (or a numeric id in the same range) before their value.
const MAPI_NAMED_PROPERTY: u16 = 0x8000;

/// Errors from [`parse`] and the RTF helpers. Decoding is best-effort by
/// design - a TNEF part that fails to decode leaves its raw `winmail.dat`
/// attachment in place rather than hiding the message's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TnefError {
    /// The stream does not begin with the TNEF signature.
    NotTnef,
    /// The stream ended - or an attribute's declared length ran past the end
    /// of the data - before the stream was complete.
    Truncated,
    /// The stream carried no complete attributes at all.
    Empty,
    /// A compressed-RTF payload was declared with a compression type this
    /// decoder does not know.
    UnsupportedRtfCompression,
    /// A compressed-RTF payload failed decompression (a size header that
    /// overflows, or a CRC that does not match).
    Rtf(String),
}

impl fmt::Display for TnefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TnefError::NotTnef => write!(f, "not a TNEF stream"),
            TnefError::Truncated => write!(f, "truncated TNEF stream"),
            TnefError::Empty => write!(f, "TNEF stream carries no attributes"),
            TnefError::UnsupportedRtfCompression => write!(f, "unsupported compressed-RTF encoding"),
            TnefError::Rtf(message) => write!(f, "compressed RTF: {message}"),
        }
    }
}

impl StdError for TnefError {}

/// One attachment carried inside a TNEF payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TnefAttachment {
    /// The display filename: the MAPI long filename when the attachment
    /// carries one, else the TNEF title, else the transport filename, else a
    /// generic name.
    pub filename: String,
    /// The attachment's MIME type: its MAPI MIME tag when present, else
    /// guessed from the filename extension, else `application/octet-stream`.
    pub content_type: String,
    /// The attachment's bytes.
    pub data: Vec<u8>,
    /// The MAPI content ID for an inline attachment (`cid:`), if any. The
    /// mail layer renders these like inline MIME images: referenced from the
    /// HTML body and kept out of the attachment strip.
    pub cid: Option<String>,
}

/// Everything a TNEF payload holds that the reader cares about.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TnefContents {
    /// The decoded attachments, in stream order. Attachments that are really
    /// iCalendar payloads are not listed here - they surface through
    /// [`TnefContents::ics`].
    pub attachments: Vec<TnefAttachment>,
    /// The plain-text body from `attBody`/`PR_BODY`, if any.
    pub plain_body: Option<String>,
    /// The HTML body: `PR_BODY_HTML` when the stream carries it, else the
    /// de-encapsulated HTML of the RTF body when it was HTML-derived.
    pub html_body: Option<String>,
    /// An embedded iCalendar payload (an `invite.ics`-style attachment or a
    /// body that is entirely a VCALENDAR), surfaced so the reading pane's
    /// invitation banner can act on it. `None` for the vast majority.
    pub ics: Option<String>,
    /// `PR_MESSAGE_CLASS`/`attMessageClass` (`IPM.Note`, `IPM.MeetingRequest`,
    /// ...), kept for meeting-request detection.
    pub message_class: Option<String>,
    /// The `attSubject` attribute, when present.
    pub subject: Option<String>,
}

impl TnefContents {
    /// Whether the payload carries nothing this decoder can use.
    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty() && self.plain_body.is_none() && self.html_body.is_none() && self.ics.is_none()
    }
}

/// Whether `content_type` names a TNEF payload.
pub fn is_tnef_content_type(content_type: &str) -> bool {
    content_type.eq_ignore_ascii_case("application/ms-tnef")
        || content_type.eq_ignore_ascii_case("application/x-tnef")
        || content_type.eq_ignore_ascii_case("application/vnd.ms-tnef")
}

/// The synthetic `BodyPart::part_number` for the `index`-th (1-based)
/// attachment inside the TNEF part numbered `source_part`. The mail session's
/// `FetchAttachment` handler recognises this scheme and decodes the source
/// TNEF part instead of asking the server for a MIME section.
pub fn tnef_part_number(source_part: &str, index: usize) -> String {
    format!("tnef:{source_part}:{index}")
}

/// Parses a synthetic part number back into its `(source part, index)`, or
/// `None` when `part_number` names an ordinary MIME section. `index` is
/// 1-based.
pub fn tnef_attachment_ref(part_number: &str) -> Option<(&str, usize)> {
    let rest = part_number.strip_prefix("tnef:")?;
    let (source, index) = rest.rsplit_once(':')?;
    if source.is_empty() {
        return None;
    }
    let index: usize = index.parse().ok()?;
    (index >= 1).then_some((source, index))
}

/// Decodes a TNEF stream into its usable contents. Never panics on malformed
/// input: structural damage (a truncated attribute, a bogus length) stops the
/// attribute walk and returns whatever was decoded up to that point, while a
/// stream that isn't TNEF at all is a [`TnefError::NotTnef`].
///
/// Attribute checksums are deliberately not enforced: mail gateways routinely
/// rewrite payloads without recomputing them, and rejecting on mismatch would
/// throw away decodable attachments. Individual payload failures (a bad RTF
/// blob) merely leave that piece `None`.
pub fn parse(bytes: &[u8]) -> Result<TnefContents, TnefError> {
    if bytes.len() < 6 || u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) != SIGNATURE {
        return Err(TnefError::NotTnef);
    }

    let mut out = TnefContents::default();
    let mut codepage: Option<u32> = None;
    let mut rtf_compressed: Option<Vec<u8>> = None;
    let mut mapi_props: Option<Vec<u8>> = None;
    let mut current: Option<RawAttachment> = None;
    let mut raw_attachments: Vec<RawAttachment> = Vec::new();
    let mut attributes = 0usize;

    // Attribute layout ([MS-OXTNEF] §2.1): level (1), name (2), type (2),
    // length (4), value (length), checksum (2). The header is 9 bytes and the
    // checksum always trails the value, including zero-length values.
    let mut pos = 6usize;
    while pos + 11 <= bytes.len() {
        let level = bytes[pos];
        let name = u16::from_le_bytes([bytes[pos + 1], bytes[pos + 2]]);
        let length = u32::from_le_bytes([bytes[pos + 5], bytes[pos + 6], bytes[pos + 7], bytes[pos + 8]]) as usize;
        let value_start = pos + 9;
        let Some(value_end) = value_start.checked_add(length) else { break };
        if value_end > bytes.len() {
            break;
        }
        let value = &bytes[value_start..value_end];
        attributes += 1;

        match name {
            ATT_OEM_CODEPAGE => codepage = read_u32(value, 0),
            ATT_TNEF_VERSION => {}
            ATT_SUBJECT => out.subject = out.subject.or_else(|| Some(decode_text(value, codepage))),
            ATT_MESSAGE_CLASS => out.message_class = out.message_class.or_else(|| Some(decode_text(value, codepage))),
            ATT_BODY => out.plain_body = out.plain_body.or_else(|| Some(decode_text(value, codepage))),
            ATT_RTF_COMPRESSED => rtf_compressed = rtf_compressed.or_else(|| Some(value.to_vec())),
            ATT_MAPI_PROPS => mapi_props = mapi_props.or_else(|| Some(value.to_vec())),
            ATT_ATTACH_RENDDATA => {
                finalize_attachment(&mut current, &mut raw_attachments);
                current = Some(RawAttachment::default());
            }
            ATT_ATTACH_DATA => current.get_or_insert_with(RawAttachment::default).data = Some(value.to_vec()),
            ATT_ATTACH_TITLE => current.get_or_insert_with(RawAttachment::default).title = Some(value.to_vec()),
            ATT_ATTACH_META_FILE => current.get_or_insert_with(RawAttachment::default).meta = Some(value.to_vec()),
            ATT_ATTACH_TRANSPORT_FILENAME => current.get_or_insert_with(RawAttachment::default).transport_filename = Some(value.to_vec()),
            ATT_ATTACHMENT => {
                if let Some(slot) = current.as_mut() {
                    slot.props = Some(value.to_vec());
                }
                finalize_attachment(&mut current, &mut raw_attachments);
            }
            // Message-level attributes with no reader here: from/owner ids,
            // dates, recipient table, priority, status. `level` is otherwise
            // unused: the attachment attribute names are distinct from the
            // message ones, which is more reliable than the level byte (some
            // senders get it wrong).
            _ => {
                let _ = level;
            }
        }

        // The trailing checksum is always present; an attribute that is the
        // file's last may be missing it in a truncated capture.
        pos = match value_end.checked_add(2) {
            Some(next) if next <= bytes.len() => next,
            _ => break,
        };
    }
    finalize_attachment(&mut current, &mut raw_attachments);

    if attributes == 0 {
        return Err(TnefError::Empty);
    }

    // MAPI message properties can refine any of the above: the rich body,
    // and an internet codepage that controls how body/filename bytes decode.
    let mut mapi_codepage = None;
    if let Some(props) = mapi_props.as_deref() {
        let properties = mapi_properties(props);
        if let Some(value) = mapi_int(&properties, MAPI_INTERNET_CODEPAGE) {
            mapi_codepage = Some(value);
        }
        if out.message_class.is_none() {
            out.message_class = mapi_string(&properties, MAPI_MESSAGE_CLASS, codepage);
        }
        if out.plain_body.is_none() {
            out.plain_body = mapi_body(&properties, MAPI_BODY, codepage);
        }
        if rtf_compressed.is_none() {
            rtf_compressed = mapi_bytes(&properties, MAPI_RTF_COMPRESSED).map(<[u8]>::to_vec);
        }
        out.html_body = mapi_body(&properties, MAPI_BODY_HTML, mapi_codepage.or(codepage));
    }

    // The RTF body: decompress, and de-encapsulate when it was HTML-derived.
    if out.html_body.is_none() {
        if let Some(compressed) = rtf_compressed.as_deref() {
            if let Ok(rtf) = rtf_decompress(compressed) {
                out.html_body = rtf_deencapsulate_html(&rtf);
            }
        }
    }

    // Attachments: decode names, types and bytes; iCalendar payloads become
    // the `ics` field instead of a listed attachment, mirroring how MIME
    // `text/calendar` parts are treated.
    for raw in raw_attachments {
        let Some(data) = raw.data else { continue };
        let props = raw.props.as_deref().map(mapi_properties).unwrap_or_default();
        let mime_tag = mapi_string(&props, MAPI_ATTACH_MIME_TAG, codepage);
        let long_filename = mapi_string(&props, MAPI_ATTACH_LONG_FILENAME, codepage);
        let short_filename = mapi_string(&props, MAPI_ATTACH_FILENAME, codepage);
        let cid = mapi_string(&props, MAPI_ATTACH_CONTENT_ID, codepage)
            .map(|cid| cid.trim().trim_matches(|c| c == '<' || c == '>').to_string())
            .filter(|cid| !cid.is_empty());
        let filename = long_filename
            .or_else(|| raw.title.as_deref().map(|title| decode_text(title, codepage)))
            .or(short_filename)
            .or_else(|| raw.transport_filename.as_deref().map(|name| decode_text(name, codepage)))
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "attachment".to_string());
        let content_type = mime_tag
            .filter(|tag| !tag.trim().is_empty())
            .map(|tag| tag.trim().to_ascii_lowercase())
            .unwrap_or_else(|| content_type_for(&filename));

        if (content_type == "text/calendar" || is_ics_filename(&filename)) && out.ics.is_none() && contains_ics(&data) {
            out.ics = Some(decode_text(&data, mapi_codepage.or(codepage)));
            continue;
        }

        out.attachments.push(TnefAttachment {
            filename,
            content_type,
            data,
            cid,
        });
    }

    // Some senders put the meeting request in the body rather than an
    // attachment; surface a body that is entirely a VCALENDAR too.
    if out.ics.is_none() {
        if let Some(body) = out.plain_body.as_deref() {
            out.ics = extract_ics(body);
            if out.ics.is_some() {
                out.plain_body = None;
            }
        }
    }

    Ok(out)
}

/// Attachment data accumulated between `attAttachRenddata` and the
/// terminating `attAttachment` attribute.
#[derive(Default)]
struct RawAttachment {
    data: Option<Vec<u8>>,
    title: Option<Vec<u8>>,
    meta: Option<Vec<u8>>,
    transport_filename: Option<Vec<u8>>,
    props: Option<Vec<u8>>,
}

fn finalize_attachment(current: &mut Option<RawAttachment>, out: &mut Vec<RawAttachment>) {
    if let Some(attachment) = current.take() {
        if attachment.data.is_some() {
            out.push(attachment);
        }
    }
}

/// Reads a little-endian `u32` at `offset`, if the bytes are there.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes.get(offset..offset + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// A MAPI property from a property stream: its id, its type, and its raw
/// values (for the fixed-size types each value is the type's bytes; for the
/// variable types each value is the content with padding removed).
struct MapiProperty {
    id: u16,
    type_: u16,
    values: Vec<Vec<u8>>,
}

/// Walks a MAPI property stream ([MS-OXCDATA] §2.9.3): a `u32` count, then
/// each property's tag (`u16` type + `u16` id), named-property decoration,
/// multi-value count, and values. Unknown types stop the walk - the stream is
/// sequential, so there is no way to stay in sync past one.
fn mapi_properties(data: &[u8]) -> Vec<MapiProperty> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let Some(count) = read_u32(data, pos) else { return out };
    pos += 4;
    for _ in 0..count.min(10_000) {
        let Some(type_) = read_u16(data, pos) else { break };
        let Some(id) = read_u16(data, pos + 2) else { break };
        pos += 4;

        // Named (GUID) properties: a 16-byte GUID, then either a 4-byte
        // property id (kind 0) or a length-prefixed UTF-16 name.
        if id >= MAPI_NAMED_PROPERTY {
            let Some(kind) = read_u32(data, pos + 16) else { break };
            pos += 20;
            if kind == 0 {
                pos += 4;
            } else {
                let Some(name_len) = read_u32(data, pos) else { break };
                pos += 4 + (name_len as usize).div_ceil(4) * 4;
            }
        }

        let multi = type_ & MAPI_TYPE_MULTI_VALUE != 0;
        let base_type = type_ & !MAPI_TYPE_MULTI_VALUE;
        let variable = matches!(
            base_type,
            MAPI_TYPE_STRING | MAPI_TYPE_UNICODE_STRING | MAPI_TYPE_OBJECT | MAPI_TYPE_BINARY | MAPI_TYPE_UNSPECIFIED
        );
        // A multi-value property's count is already read; a variable-length
        // property carries its own value count even when it isn't multi
        // (invariably 1); fixed-size properties hold exactly one value.
        let value_count = if multi || variable {
            let Some(n) = read_u32(data, pos) else { break };
            pos += 4;
            n as usize
        } else {
            1
        };
        let mut values = Vec::new();
        let mut failed = false;
        for _ in 0..value_count.min(100_000) {
            match base_type {
                MAPI_TYPE_SHORT | MAPI_TYPE_BOOLEAN => match take(data, &mut pos, 2) {
                    Some(value) => values.push(value.to_vec()),
                    None => {
                        failed = true;
                        break;
                    }
                },
                MAPI_TYPE_INT | MAPI_TYPE_FLOAT | MAPI_TYPE_ERROR => match take(data, &mut pos, 4) {
                    Some(value) => values.push(value.to_vec()),
                    None => {
                        failed = true;
                        break;
                    }
                },
                MAPI_TYPE_DOUBLE | MAPI_TYPE_CURRENCY | MAPI_TYPE_APPTIME | MAPI_TYPE_INT8BYTE | MAPI_TYPE_SYSTIME => match take(data, &mut pos, 8) {
                    Some(value) => values.push(value.to_vec()),
                    None => {
                        failed = true;
                        break;
                    }
                },
                MAPI_TYPE_CLSID => match take(data, &mut pos, 16) {
                    Some(value) => values.push(value.to_vec()),
                    None => {
                        failed = true;
                        break;
                    }
                },
                MAPI_TYPE_STRING | MAPI_TYPE_UNICODE_STRING | MAPI_TYPE_OBJECT | MAPI_TYPE_BINARY | MAPI_TYPE_UNSPECIFIED => {
                    let Some(length) = read_u32(data, pos) else {
                        failed = true;
                        break;
                    };
                    let length = length as usize;
                    pos += 4;
                    let padded = length.div_ceil(4) * 4;
                    match data.get(pos..pos + length) {
                        Some(value) if pos + padded <= data.len() => {
                            values.push(value.to_vec());
                            pos += padded;
                        }
                        _ => {
                            failed = true;
                            break;
                        }
                    }
                }
                // An unknown type ends the stream's readable portion.
                _ => {
                    failed = true;
                    break;
                }
            }
        }
        if multi && matches!(base_type, MAPI_TYPE_SHORT | MAPI_TYPE_BOOLEAN) && value_count % 2 == 1 {
            pos += 2;
        }
        if failed {
            break;
        }
        out.push(MapiProperty { id, type_, values });
    }
    out
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes.get(offset..offset + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, length: usize) -> Option<&'a [u8]> {
    let value = bytes.get(*pos..*pos + length)?;
    *pos += length;
    Some(value)
}

fn mapi_property(properties: &[MapiProperty], id: u16) -> Option<&MapiProperty> {
    properties.iter().find(|property| property.id == id)
}

fn mapi_bytes(properties: &[MapiProperty], id: u16) -> Option<&[u8]> {
    mapi_property(properties, id)?.values.first().map(Vec::as_slice)
}

/// A MAPI string property decoded according to its declared type: unicode
/// strings are UTF-16LE, everything else follows `codepage`.
fn mapi_string(properties: &[MapiProperty], id: u16, codepage: Option<u32>) -> Option<String> {
    let property = mapi_property(properties, id)?;
    let value = property.values.first()?;
    let text = if property.type_ & !MAPI_TYPE_MULTI_VALUE == MAPI_TYPE_UNICODE_STRING {
        decode_utf16(value)
    } else {
        decode_text(value, codepage)
    };
    (!text.is_empty()).then_some(text)
}

/// A MAPI body property decoded according to its declared type. HTML bodies
/// arrive as unicode strings, ANSI strings or raw binary depending on the
/// Outlook version, so all three are tolerated.
fn mapi_body(properties: &[MapiProperty], id: u16, codepage: Option<u32>) -> Option<String> {
    let property = mapi_property(properties, id)?;
    let value = property.values.first()?;
    let text = if property.type_ & !MAPI_TYPE_MULTI_VALUE == MAPI_TYPE_UNICODE_STRING {
        decode_utf16(value)
    } else {
        decode_text(value, codepage)
    };
    (!text.trim().is_empty()).then_some(text)
}

fn mapi_int(properties: &[MapiProperty], id: u16) -> Option<u32> {
    read_u32(mapi_bytes(properties, id)?, 0)
}

/// Decodes text bytes into a `String`, tolerating the encodings Outlook
/// actually emits: UTF-16LE (a declared codepage of 1200, or a byte pattern
/// that is mostly NULs in odd positions), UTF-8, else a Windows-1252/Latin-1
/// fallback. Trailing NUL terminators are stripped.
fn decode_text(bytes: &[u8], codepage: Option<u32>) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // Test for UTF-16 before stripping NUL terminators: a Latin-script UTF-16
    // string ends in 00 00, and greedily stripping trailing NULs would eat
    // the high zero of the final character too.
    if codepage == Some(1200) || looks_utf16le(bytes) {
        return decode_utf16(bytes);
    }
    let bytes = strip_trailing_nuls(bytes);
    if bytes.is_empty() {
        return String::new();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    windows_1252(bytes)
}

fn strip_trailing_nuls(mut bytes: &[u8]) -> &[u8] {
    while let Some((last, rest)) = bytes.split_last() {
        if *last == 0 {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

/// Whether a byte run looks like UTF-16LE: even length and most odd-position
/// bytes zero (Latin-script text), which never happens for a UTF-8/cp1252
/// string that contains no NULs.
fn looks_utf16le(bytes: &[u8]) -> bool {
    if bytes.len() < 4 || !bytes.len().is_multiple_of(2) {
        return false;
    }
    let pairs = bytes.len() / 2;
    let zero_highs = bytes.chunks_exact(2).filter(|pair| pair[1] == 0).count();
    zero_highs * 2 >= pairs
}

fn decode_utf16(bytes: &[u8]) -> String {
    let mut units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16_lossy(&units)
}

/// The Windows-1252 characters in 0x80..=0x9F, where cp1252 differs from
/// Latin-1; everything else maps straight to its code point.
fn windows_1252(bytes: &[u8]) -> String {
    const PUNCTUATION: [char; 32] = [
        '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}',
        '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    ];
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9F => PUNCTUATION[(byte - 0x80) as usize],
            _ => byte as char,
        })
        .collect()
}

/// Whether a filename claims to be an iCalendar document.
fn is_ics_filename(filename: &str) -> bool {
    filename
        .rsplit('.')
        .next()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ics") || extension.eq_ignore_ascii_case("icalendar"))
}

/// Whether `bytes` contain an iCalendar document.
fn contains_ics(bytes: &[u8]) -> bool {
    bytes.windows(15).any(|window| window.eq_ignore_ascii_case(b"BEGIN:VCALENDAR"))
}

/// A body that is entirely (or substantially) a VCALENDAR, extracted so the
/// invitation banner can act on it.
fn extract_ics(text: &str) -> Option<String> {
    let start = text.find("BEGIN:VCALENDAR")?;
    let end = text[start..].find("END:VCALENDAR")? + start + "END:VCALENDAR".len();
    Some(text[start..end].to_string())
}

/// The MIME type guessed from a filename extension, used when the attachment
/// carries no MAPI MIME tag.
fn content_type_for(filename: &str) -> String {
    let extension = filename.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase()).unwrap_or_default();
    let guessed = match extension.as_str() {
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "ics" | "icalendar" => "text/calendar",
        "vcf" => "text/vcard",
        "eml" => "message/rfc822",
        "rtf" => "application/rtf",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "msg" => "application/vnd.ms-outlook",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "mp3" => "audio/mpeg",
        "wav" => "audio/x-wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    };
    guessed.to_string()
}

/// Decompresses a compressed-RTF payload ([MS-OXRTFCP]): the 16-byte header
/// (`compressed size`, `uncompressed size`, the `LZFu`/`MELA` magic, CRC-32),
/// then the LZ77 token stream. The size fields are advisory only - decoding
/// stops at the payload's end or the end marker - but a CRC mismatch on a
/// compressed payload is an error rather than yielding garbage.
pub fn rtf_decompress(input: &[u8]) -> Result<Vec<u8>, TnefError> {
    if input.len() < 16 {
        return Err(TnefError::Rtf("header is shorter than 16 bytes".to_string()));
    }
    let compressed_size = u32::from_le_bytes([input[0], input[1], input[2], input[3]]) as usize;
    let raw_size = u32::from_le_bytes([input[4], input[5], input[6], input[7]]) as usize;
    let magic = &input[8..12];
    let crc = u32::from_le_bytes([input[12], input[13], input[14], input[15]]);
    // `compressed_size` counts the header's last 12 bytes plus the payload
    // ([MS-OXRTFCP] §2.4.1), so the payload is everything after the 16-byte
    // header up to `compressed_size` minus those 12.
    let payload_len = compressed_size.saturating_sub(12).min(input.len() - 16);
    let payload = &input[16..16 + payload_len];

    match magic {
        b"LZFu" => {
            if crc32(payload) != crc {
                return Err(TnefError::Rtf("CRC mismatch".to_string()));
            }
            decompress_lzfu(payload, raw_size)
        }
        b"MELA" => Ok(payload[..raw_size.min(payload.len())].to_vec()),
        _ => Err(TnefError::UnsupportedRtfCompression),
    }
}

/// The LZ77 variant [MS-OXRTFCP] uses: an 8-bit control byte whose bits are
/// read least-significant-first selects literals (bit 0) or 16-bit
/// big-endian back-references (bit 1); a reference whose offset equals the
/// write position is the end marker. The 4096-byte window starts preloaded
/// with a fixed RTF preamble and is padded with spaces.
fn decompress_lzfu(payload: &[u8], raw_size: usize) -> Result<Vec<u8>, TnefError> {
    let mut dictionary = [b' '; RTF_DICT_SIZE];
    dictionary[..RTF_INIT_DICT.len()].copy_from_slice(RTF_INIT_DICT);
    let mut write_offset = RTF_INIT_DICT.len();
    let mut out: Vec<u8> = Vec::with_capacity(raw_size.min(1 << 20));
    let mut pos = 0usize;

    while pos < payload.len() && out.len() < raw_size {
        let control = payload[pos];
        pos += 1;
        for bit in 0..8 {
            if pos >= payload.len() || out.len() >= raw_size {
                break;
            }
            if control & (1 << bit) == 0 {
                let byte = payload[pos];
                pos += 1;
                out.push(byte);
                dictionary[write_offset] = byte;
                write_offset = (write_offset + 1) % RTF_DICT_SIZE;
            } else {
                if pos + 2 > payload.len() {
                    break;
                }
                let token = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
                pos += 2;
                let offset = (token >> 4) & 0x0FFF;
                if offset == write_offset {
                    return Ok(out);
                }
                let length = (token & 0x0F) + 2;
                for step in 0..length {
                    let byte = dictionary[(offset + step) % RTF_DICT_SIZE];
                    out.push(byte);
                    dictionary[write_offset] = byte;
                    write_offset = (write_offset + 1) % RTF_DICT_SIZE;
                }
            }
        }
    }
    Ok(out)
}

/// The CRC-32 [MS-OXRTFCP] stores in its header: the standard reflected
/// polynomial (`0xEDB88320`, initial `0xFFFFFFFF`, final complement).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// De-encapsulates the HTML embedded in an RTF body ([MS-OXRTFEX]).
///
/// An RTF body produced from HTML carries `\fromhtml1` in its header, wraps
/// each HTML tag in `{\*\htmltag ...}` destinations, and marks RTF-only
/// rendering with `\htmlrtf` ... `\htmlrtf0` toggles. De-encapsulation walks
/// the token stream and emits only the HTML: tag destination contents, and
/// ordinary text outside any `\htmlrtf` stretch, with `\par`/`\line` as
/// newlines and the RTF escapes decoded. Everything else - font/colour
/// tables, `\pntext` numbering, generator metadata, RTF-only spans - is
/// suppressed. Returns `None` when the document isn't HTML-derived, so the
/// caller never renders raw RTF.
pub fn rtf_deencapsulate_html(rtf: &[u8]) -> Option<String> {
    if !rtf[..rtf.len().min(1024)].windows(9).any(|window| window == b"\\fromhtml") {
        return None;
    }
    let tokens = tokenize_rtf(rtf);
    let mut out = String::new();
    let mut stack: Vec<Frame> = vec![Frame {
        kind: FrameKind::Content,
        htmlrtf: false,
        uc_skip: 1,
    }];
    let mut htmlrtf = false;
    let mut suppression = 0usize;
    let mut htmltag_depth = 0usize;
    let mut uc_skip = 1usize;
    let mut pending_skip = 0usize;

    for token in tokens {
        match token {
            RtfToken::GroupStart => stack.push(Frame {
                kind: FrameKind::Pending,
                htmlrtf,
                uc_skip,
            }),
            RtfToken::GroupEnd => {
                if stack.len() <= 1 {
                    continue;
                }
                let frame = stack.pop().expect("len checked");
                htmlrtf = frame.htmlrtf;
                uc_skip = frame.uc_skip;
                match frame.kind {
                    FrameKind::HtmlTag => htmltag_depth = htmltag_depth.saturating_sub(1),
                    FrameKind::Skip => suppression = suppression.saturating_sub(1),
                    _ => {}
                }
            }
            RtfToken::Symbol(b'*') => {
                if let Some(frame) = stack.last_mut() {
                    if frame.kind == FrameKind::Pending {
                        frame.kind = FrameKind::StarredDestination;
                    }
                }
            }
            RtfToken::Symbol(symbol) => {
                if pending_skip > 0 {
                    pending_skip -= 1;
                    continue;
                }
                if suppression > 0 || htmlrtf {
                    continue;
                }
                match symbol {
                    b'\\' | b'{' | b'}' => out.push(symbol as char),
                    b'~' => out.push('\u{a0}'),
                    b'_' => out.push('\u{2011}'),
                    // `\-` (optional hyphen) and any stray control symbol
                    // have no HTML representation.
                    _ => {}
                }
            }
            RtfToken::Control { word, param } => {
                if let Some(frame) = stack.last_mut() {
                    match frame.kind {
                        FrameKind::Pending => {
                            frame.kind = if is_htmltag_word(&word) {
                                FrameKind::HtmlTag
                            } else if is_destination(&word) {
                                FrameKind::Skip
                            } else {
                                FrameKind::Content
                            };
                            if frame.kind == FrameKind::HtmlTag {
                                htmltag_depth += 1;
                            } else if frame.kind == FrameKind::Skip {
                                suppression += 1;
                            }
                        }
                        FrameKind::StarredDestination => {
                            frame.kind = if is_htmltag_word(&word) { FrameKind::HtmlTag } else { FrameKind::Skip };
                            if frame.kind == FrameKind::HtmlTag {
                                htmltag_depth += 1;
                            } else {
                                suppression += 1;
                            }
                        }
                        _ => {}
                    }
                }
                let emitting = htmltag_depth > 0 || suppression == 0;
                match word.as_str() {
                    // `\htmlrtf0` tokenizes as the word `htmlrtf` with the
                    // parameter 0.
                    "htmlrtf" => htmlrtf = param != Some(0),
                    "uc" => {
                        if let Some(value) = param {
                            uc_skip = value.max(0) as usize;
                        }
                    }
                    "u" => {
                        if emitting && !htmlrtf {
                            if let Some(value) = param {
                                let code = if value < 0 { value + 0x1_0000 } else { value };
                                if let Some(character) = char::from_u32(code as u32) {
                                    out.push(character);
                                }
                            }
                        }
                        pending_skip = uc_skip;
                    }
                    "par" | "line" => {
                        if emitting && !htmlrtf {
                            out.push('\n');
                        }
                    }
                    "tab" if emitting && !htmlrtf => {
                        out.push('\t');
                    }
                    _ => {}
                }
            }
            RtfToken::Hex(byte) => {
                if pending_skip > 0 {
                    pending_skip -= 1;
                    continue;
                }
                if suppression == 0 && !htmlrtf {
                    out.push(windows_1252(&[byte]).chars().next().unwrap_or('\u{FFFD}'));
                }
            }
            RtfToken::Text(text) => {
                if suppression > 0 || htmlrtf {
                    continue;
                }
                if pending_skip > 0 {
                    let mut characters = text.chars();
                    let mut rest = String::new();
                    for _ in 0..pending_skip {
                        if characters.next().is_none() {
                            break;
                        }
                    }
                    rest.extend(characters);
                    pending_skip = 0;
                    out.push_str(&rest);
                } else {
                    out.push_str(&text);
                }
            }
        }
    }

    (!out.trim().is_empty()).then_some(out)
}

/// A group on the de-encapsulation stack: what it is, and the `\htmlrtf` and
/// `\ucN` state to restore when it closes (RTF state is group-scoped).
struct Frame {
    kind: FrameKind,
    htmlrtf: bool,
    uc_skip: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// A group whose first token hasn't been seen yet.
    Pending,
    /// `{\*` seen, waiting for the destination name.
    StarredDestination,
    /// An `{\*\htmltag ...}` destination: emitted as HTML.
    HtmlTag,
    /// A destination whose contents are suppressed (font tables, metadata).
    Skip,
    /// An ordinary formatting group.
    Content,
}

/// Control words that make a group a destination ([MS-OXRTFEX] §2.2 reads
/// the HTML out of exactly one of them; the rest are RTF plumbing whose
/// contents must not leak into the HTML).
fn is_destination(word: &str) -> bool {
    matches!(
        word,
        "fonttbl"
            | "colortbl"
            | "stylesheet"
            | "info"
            | "pict"
            | "header"
            | "headerl"
            | "headerr"
            | "headerf"
            | "footer"
            | "footerl"
            | "footerr"
            | "footerf"
            | "pntext"
            | "pn"
            | "listtext"
            | "listtable"
            | "filetbl"
            | "revtbl"
            | "xmlnstbl"
            | "generator"
            | "themedata"
            | "colorschememapping"
            | "datastore"
            | "latentstyles"
            | "shppict"
            | "nonshppict"
            | "object"
            | "objdata"
            | "objclass"
            | "bkmkstart"
            | "bkmkend"
            | "field"
            | "fldinst"
            | "xmlopen"
    )
}

/// `\htmltag` or `\htmltag<N>` - the tag-number form also names the
/// destination.
fn is_htmltag_word(word: &str) -> bool {
    word == "htmltag"
        || word
            .strip_prefix("htmltag")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

enum RtfToken {
    GroupStart,
    GroupEnd,
    /// A control word with its optional numeric parameter.
    Control {
        word: String,
        param: Option<i32>,
    },
    /// A control symbol: the single character after the backslash.
    Symbol(u8),
    /// `\'XX` - an encoded byte.
    Hex(u8),
    /// A run of literal text.
    Text(String),
}

/// Splits RTF into the tokens the de-encapsulator needs. The input is treated
/// as bytes: RTF is byte-oriented, and the 8-bit `\'XX` escapes are decoded
/// through the Windows-1252 fallback table.
fn tokenize_rtf(rtf: &[u8]) -> Vec<RtfToken> {
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut text: Vec<u8> = Vec::new();

    fn flush_text(tokens: &mut Vec<RtfToken>, text: &mut Vec<u8>) {
        if !text.is_empty() {
            tokens.push(RtfToken::Text(String::from_utf8_lossy(text).into_owned()));
            text.clear();
        }
    }

    while i < rtf.len() {
        match rtf[i] {
            b'{' => {
                flush_text(&mut tokens, &mut text);
                tokens.push(RtfToken::GroupStart);
                i += 1;
            }
            b'}' => {
                flush_text(&mut tokens, &mut text);
                tokens.push(RtfToken::GroupEnd);
                i += 1;
            }
            b'\\' => {
                flush_text(&mut tokens, &mut text);
                i += 1;
                let Some(&symbol) = rtf.get(i) else { break };
                if symbol.is_ascii_alphabetic() {
                    let start = i;
                    while i < rtf.len() && rtf[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word = String::from_utf8_lossy(&rtf[start..i]).to_ascii_lowercase();
                    let mut param = None;
                    let mut negative = false;
                    if i < rtf.len() && rtf[i] == b'-' {
                        negative = true;
                        i += 1;
                    }
                    let digits_start = i;
                    while i < rtf.len() && rtf[i].is_ascii_digit() {
                        i += 1;
                    }
                    if i > digits_start {
                        if let Ok(value) = String::from_utf8_lossy(&rtf[digits_start..i]).parse::<i32>() {
                            param = Some(if negative { -value } else { value });
                        }
                    }
                    // A single space after a control word is its delimiter.
                    if i < rtf.len() && rtf[i] == b' ' {
                        i += 1;
                    }
                    tokens.push(RtfToken::Control { word, param });
                } else if symbol == b'\'' {
                    let hex = rtf
                        .get(i + 1..i + 3)
                        .and_then(|pair| std::str::from_utf8(pair).ok())
                        .and_then(|pair| u8::from_str_radix(pair, 16).ok());
                    match hex {
                        Some(byte) => {
                            tokens.push(RtfToken::Hex(byte));
                            i += 3;
                        }
                        None => {
                            tokens.push(RtfToken::Symbol(b'\''));
                            i += 1;
                        }
                    }
                } else {
                    tokens.push(RtfToken::Symbol(symbol));
                    i += 1;
                }
            }
            byte => {
                text.push(byte);
                i += 1;
            }
        }
    }
    flush_text(&mut tokens, &mut text);
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// The sample RTF body used by the fixture: an HTML-derived document
    /// exercising a tag destination, an RTF-only span, bold text and a
    /// character escape.
    fn sample_rtf() -> Vec<u8> {
        br"{\rtf1\ansi\ansicpg1252\fromhtml1\deff0{\fonttbl{\f0\fswiss Arial;}}\viewkind4\uc1\pard\f0\fs22{\*\htmltag <html><body><p>}{\htmlrtf \f0 \htmlrtf0 Hello }{\*\htmltag <b>}{\htmlrtf \b \htmlrtf0 world{\*\htmltag </b>}}{\*\htmltag </p></body></html>}".to_vec()
    }

    fn sample_plain_body() -> &'static str {
        "Plain body from TNEF."
    }

    fn sample_html_body() -> &'static str {
        "<html><body><p>Hello <b>world</b></p></body></html>"
    }

    const SAMPLE_ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Lookout//Synthetic TNEF fixture//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:fixture@lookout.test\r\nDTSTART:20261014T090000Z\r\nDTEND:20261014T100000Z\r\nSUMMARY:Synthetic TNEF invitation\r\nEND:VEVENT\r\nEND:VCALENDAR";

    /// Encodes one TNEF attribute: level, name, type, length, value, checksum.
    fn attribute(level: u8, name: u16, type_: u16, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(level);
        out.extend_from_slice(&name.to_le_bytes());
        out.extend_from_slice(&type_.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
        let checksum = value.iter().map(|&byte| byte as u16).fold(0u16, u16::wrapping_add);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Encodes one MAPI property into a property stream.
    fn mapi_property(id: u16, type_: u16, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&type_.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        match type_ {
            MAPI_TYPE_SHORT | MAPI_TYPE_BOOLEAN | MAPI_TYPE_INT | MAPI_TYPE_FLOAT | MAPI_TYPE_ERROR | MAPI_TYPE_DOUBLE | MAPI_TYPE_CURRENCY | MAPI_TYPE_APPTIME
            | MAPI_TYPE_INT8BYTE | MAPI_TYPE_SYSTIME | MAPI_TYPE_CLSID => out.extend_from_slice(value),
            _ => {
                out.extend_from_slice(&1u32.to_le_bytes());
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                out.extend_from_slice(value);
                let padding = value.len().div_ceil(4) * 4 - value.len();
                out.extend(std::iter::repeat_n(0u8, padding));
            }
        }
        out
    }

    fn mapi_stream(properties: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(properties.len() as u32).to_le_bytes());
        for (id, type_, value) in properties {
            out.extend_from_slice(&mapi_property(*id, *type_, value));
        }
        out
    }

    fn utf16(text: &str) -> Vec<u8> {
        let mut out: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn ascii_nul(text: &str) -> Vec<u8> {
        let mut out = text.as_bytes().to_vec();
        out.push(0);
        out
    }

    /// A minimal but complete LZFu compressor, mirroring [MS-OXRTFCP]'s
    /// token stream, so the fixture and the round-trip tests don't rely on a
    /// captured binary blob.
    fn compress_lzfu(data: &[u8]) -> Vec<u8> {
        let mut dictionary = [b' '; RTF_DICT_SIZE];
        dictionary[..RTF_INIT_DICT.len()].copy_from_slice(RTF_INIT_DICT);
        let mut write = RTF_INIT_DICT.len();
        let mut payload = Vec::new();
        let mut control = 0u8;
        let mut bit = 0u32;
        let mut tokens: Vec<u8> = Vec::new();
        let mut i = 0usize;

        while i < data.len() {
            let max_len = (data.len() - i).min(17);
            let mut best_len = 0usize;
            let mut best_offset = 0usize;
            for start in 0..RTF_DICT_SIZE {
                if start % RTF_DICT_SIZE == write % RTF_DICT_SIZE {
                    // A reference with offset == write position is the end
                    // marker, never a match.
                    continue;
                }
                let mut length = 0usize;
                while length < max_len && dictionary[(start + length) % RTF_DICT_SIZE] == data[i + length] {
                    length += 1;
                }
                if length > best_len {
                    best_len = length;
                    best_offset = start;
                }
            }
            if best_len >= 2 {
                control |= 1 << bit;
                bit += 1;
                let token = ((best_offset as u16 & 0x0FFF) << 4) | ((best_len as u16 - 2) & 0x0F);
                tokens.extend_from_slice(&token.to_be_bytes());
                for &byte in &data[i..i + best_len] {
                    dictionary[write] = byte;
                    write = (write + 1) % RTF_DICT_SIZE;
                }
                i += best_len;
            } else {
                bit += 1;
                tokens.push(data[i]);
                dictionary[write] = data[i];
                write = (write + 1) % RTF_DICT_SIZE;
                i += 1;
            }
            if bit == 8 {
                payload.push(control);
                payload.extend_from_slice(&tokens);
                control = 0;
                bit = 0;
                tokens.clear();
            }
        }
        if bit > 0 {
            payload.push(control);
            payload.extend_from_slice(&tokens);
        }

        let mut out = Vec::new();
        out.extend_from_slice(&((payload.len() as u32) + 12).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"LZFu");
        out.extend_from_slice(&crc32(&payload).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// The synthetic `winmail.dat` used for the checked-in fixture and the
    /// parser tests: the message bodies (plain, RTF, HTML), a text
    /// attachment, an inline image with a Content-ID and an iCalendar
    /// attachment.
    pub(crate) fn sample_winmail_dat() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SIGNATURE.to_le_bytes());
        out.extend_from_slice(&0x1234u16.to_le_bytes());

        out.extend_from_slice(&attribute(1, ATT_TNEF_VERSION, 0x0008, &0x0001_0000u32.to_le_bytes()));
        out.extend_from_slice(&attribute(1, ATT_OEM_CODEPAGE, 0x0006, &[0xE4, 0x04, 0, 0, 0, 0, 0, 0]));
        out.extend_from_slice(&attribute(1, ATT_SUBJECT, 0x0001, &ascii_nul("Synthetic TNEF message")));
        out.extend_from_slice(&attribute(1, ATT_MESSAGE_CLASS, 0x0001, &ascii_nul("IPM.Note")));
        out.extend_from_slice(&attribute(1, ATT_BODY, 0x0004, sample_plain_body().as_bytes()));

        let mapi = mapi_stream(&[
            (MAPI_MESSAGE_CLASS, MAPI_TYPE_UNICODE_STRING, utf16("IPM.Note")),
            (MAPI_BODY, MAPI_TYPE_UNICODE_STRING, utf16(sample_plain_body())),
            (MAPI_BODY_HTML, MAPI_TYPE_BINARY, sample_html_body().as_bytes().to_vec()),
            (MAPI_RTF_COMPRESSED, MAPI_TYPE_BINARY, compress_lzfu(&sample_rtf())),
            (MAPI_INTERNET_CODEPAGE, MAPI_TYPE_INT, 65001u32.to_le_bytes().to_vec()),
        ]);
        out.extend_from_slice(&attribute(1, ATT_MAPI_PROPS, 0x0006, &mapi));

        // Attachment 1: a text file, no MAPI properties.
        out.extend_from_slice(&attribute(2, ATT_ATTACH_RENDDATA, 0x0006, &[0u8; 14]));
        out.extend_from_slice(&attribute(2, ATT_ATTACH_TITLE, 0x0001, &ascii_nul("report.txt")));
        out.extend_from_slice(&attribute(2, ATT_ATTACH_TRANSPORT_FILENAME, 0x0001, &ascii_nul("REPORT.TXT")));
        out.extend_from_slice(&attribute(2, ATT_ATTACH_DATA, 0x0006, b"Quarterly report contents.\r\n"));
        out.extend_from_slice(&attribute(2, ATT_ATTACHMENT, 0x0006, &mapi_stream(&[])));

        // Attachment 2: an inline image, identified by its MAPI Content-ID.
        out.extend_from_slice(&attribute(2, ATT_ATTACH_RENDDATA, 0x0006, &[0u8; 14]));
        out.extend_from_slice(&attribute(2, ATT_ATTACH_TITLE, 0x0001, &ascii_nul("logo.png")));
        out.extend_from_slice(&attribute(2, ATT_ATTACH_DATA, 0x0006, &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        let image_props = mapi_stream(&[
            (MAPI_ATTACH_CONTENT_ID, MAPI_TYPE_UNICODE_STRING, utf16("logo123@lookout.test")),
            (MAPI_ATTACH_MIME_TAG, MAPI_TYPE_UNICODE_STRING, utf16("image/png")),
        ]);
        out.extend_from_slice(&attribute(2, ATT_ATTACHMENT, 0x0006, &image_props));

        // Attachment 3: an iCalendar invitation.
        out.extend_from_slice(&attribute(2, ATT_ATTACH_RENDDATA, 0x0006, &[0u8; 14]));
        out.extend_from_slice(&attribute(2, ATT_ATTACH_TITLE, 0x0001, &ascii_nul("invite.ics")));
        out.extend_from_slice(&attribute(2, ATT_ATTACH_DATA, 0x0006, SAMPLE_ICS.as_bytes()));
        out.extend_from_slice(&attribute(2, ATT_ATTACHMENT, 0x0006, &mapi_stream(&[])));

        out
    }

    /// The fixture `.eml` wrapping the synthetic TNEF part.
    pub(crate) fn sample_tnef_message_eml() -> Vec<u8> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(sample_winmail_dat());
        let mut body = String::new();
        body.push_str("From: Outlook User <outlook.user@example.com>\r\n");
        body.push_str("To: Lookout User <lookout.user@example.com>\r\n");
        body.push_str("Subject: Winmail.dat from Outlook (synthetic fixture)\r\n");
        body.push_str("Date: Tue, 13 Oct 2026 10:00:00 +0000\r\n");
        body.push_str("Message-ID: <tnef-fixture@lookout.test>\r\n");
        body.push_str("MIME-Version: 1.0\r\n");
        body.push_str("Content-Type: multipart/mixed; boundary=\"lookout-tnef-boundary\"\r\n");
        body.push_str("\r\n");
        body.push_str("--lookout-tnef-boundary\r\n");
        body.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        body.push_str("\r\n");
        body.push_str("Plain text part sent alongside winmail.dat.\r\n");
        body.push_str("--lookout-tnef-boundary\r\n");
        body.push_str("Content-Type: application/ms-tnef; name=\"winmail.dat\"\r\n");
        body.push_str("Content-Transfer-Encoding: base64\r\n");
        body.push_str("Content-Disposition: attachment; filename=\"winmail.dat\"\r\n");
        body.push_str("\r\n");
        for chunk in encoded.as_bytes().chunks(76) {
            body.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
            body.push_str("\r\n");
        }
        body.push_str("--lookout-tnef-boundary--\r\n");
        body.into_bytes()
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/outlook")).join(name)
    }

    #[test]
    fn rejects_a_non_tnef_stream() {
        assert_eq!(parse(b"not tnef at all"), Err(TnefError::NotTnef));
        assert_eq!(parse(&[]), Err(TnefError::NotTnef));
    }

    #[test]
    fn truncated_streams_do_not_panic() {
        let full = sample_winmail_dat();
        for cut in 0..full.len() {
            let _ = parse(&full[..cut]);
        }
    }

    #[test]
    fn parses_the_synthetic_stream() {
        let contents = parse(&sample_winmail_dat()).expect("sample parses");
        assert_eq!(contents.subject.as_deref(), Some("Synthetic TNEF message"));
        assert_eq!(contents.message_class.as_deref(), Some("IPM.Note"));
        assert_eq!(contents.plain_body.as_deref(), Some(sample_plain_body()));
        assert_eq!(contents.html_body.as_deref(), Some(sample_html_body()));
        assert!(contents.ics.as_deref().is_some_and(|ics| ics.contains("SUMMARY:Synthetic TNEF invitation")));

        assert_eq!(contents.attachments.len(), 2, "the licence attachment surfaces as ics, not a row");
        let report = &contents.attachments[0];
        assert_eq!(report.filename, "report.txt");
        assert_eq!(report.content_type, "text/plain");
        assert_eq!(report.data, b"Quarterly report contents.\r\n");
        assert_eq!(report.cid, None);

        let logo = &contents.attachments[1];
        assert_eq!(logo.filename, "logo.png");
        assert_eq!(logo.content_type, "image/png");
        assert_eq!(logo.cid.as_deref(), Some("logo123@lookout.test"));
    }

    #[test]
    fn the_checked_in_fixture_matches_the_generator() {
        let dat = std::fs::read(fixture_path("winmail.dat")).expect("winmail.dat fixture is checked in");
        assert_eq!(
            dat,
            sample_winmail_dat(),
            "test-fixtures/outlook/winmail.dat is stale - run the ignored regenerate_fixtures test"
        );
        let eml = std::fs::read(fixture_path("tnef-message.eml")).expect("tnef-message.eml fixture is checked in");
        assert_eq!(
            eml,
            sample_tnef_message_eml(),
            "test-fixtures/outlook/tnef-message.eml is stale - run the ignored regenerate_fixtures test"
        );
    }

    #[test]
    #[ignore = "regenerates the checked-in TNEF fixtures from the test-only generator"]
    fn regenerate_fixtures() {
        std::fs::write(fixture_path("winmail.dat"), sample_winmail_dat()).expect("writes winmail.dat");
        std::fs::write(fixture_path("tnef-message.eml"), sample_tnef_message_eml()).expect("writes tnef-message.eml");
    }

    #[test]
    fn parses_the_checked_in_fixture() {
        let dat = std::fs::read(fixture_path("winmail.dat")).expect("winmail.dat fixture is checked in");
        let contents = parse(&dat).expect("fixture parses");
        assert_eq!(contents.attachments.len(), 2);
        assert!(contents.html_body.is_some());
        assert!(contents.ics.is_some());
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn compressed_rtf_round_trips() {
        let compressed = compress_lzfu(&sample_rtf());
        assert_eq!(rtf_decompress(&compressed).expect("decompresses"), sample_rtf());
    }

    #[test]
    fn compressed_rtf_rejects_a_bad_crc() {
        let mut compressed = compress_lzfu(&sample_rtf());
        let last = compressed.len() - 1;
        compressed[last] ^= 0xFF;
        assert!(matches!(rtf_decompress(&compressed), Err(TnefError::Rtf(_))));
    }

    #[test]
    fn uncompressed_rtf_decodes() {
        let rtf = sample_rtf();
        let mut blob = Vec::new();
        blob.extend_from_slice(&((rtf.len() as u32) + 12).to_le_bytes());
        blob.extend_from_slice(&(rtf.len() as u32).to_le_bytes());
        blob.extend_from_slice(b"MELA");
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(&rtf);
        assert_eq!(rtf_decompress(&blob).expect("decompresses"), rtf);
    }

    #[test]
    fn deencapsulates_html_from_an_encapsulated_rtf_body() {
        let html = rtf_deencapsulate_html(&sample_rtf()).expect("html is extracted");
        assert_eq!(html, "<html><body><p>Hello <b>world</b></p></body></html>");
    }

    #[test]
    fn deencapsulation_suppresses_rtf_only_and_destination_content() {
        let rtf = br"{\rtf1\ansi\fromhtml1{\fonttbl{\f0 Arial;}}{\*\generator Riched20;}{\*\htmltag <p>}{\htmlrtf \b \htmlrtf0 visible{\pntext 1.\tab} text{\*\htmltag </p>}}";
        let html = rtf_deencapsulate_html(rtf).expect("html is extracted");
        assert_eq!(html, "<p>visible text</p>");
    }

    #[test]
    fn a_plain_rtf_body_is_not_treated_as_html() {
        let rtf = br"{\rtf1\ansi\deff0{\fonttbl{\f0 Arial;}}\pard Hello\par}";
        assert_eq!(rtf_deencapsulate_html(rtf), None);
    }

    #[test]
    fn decodes_body_codepages() {
        assert_eq!(decode_text(&utf16("Grüße"), None), "Grüße");
        assert_eq!(decode_text(&utf16("Grüße"), Some(1200)), "Grüße");
        assert_eq!(decode_text("Grüße".as_bytes(), Some(65001)), "Grüße");
        assert_eq!(decode_text(&[b'G', 0xFC, b's', b's', b'e'], None), "Güsse");
        assert_eq!(decode_text(&[0x80, 0x93, 0x94], None), "\u{20AC}\u{201C}\u{201D}");
    }

    #[test]
    fn part_number_refs_round_trip() {
        assert_eq!(tnef_part_number("2", 1), "tnef:2:1");
        assert_eq!(tnef_attachment_ref("tnef:2:1"), Some(("2", 1)));
        assert_eq!(tnef_attachment_ref("tnef:1.2:10"), Some(("1.2", 10)));
        assert_eq!(tnef_attachment_ref("2"), None);
        assert_eq!(tnef_attachment_ref("tnef:2:0"), None);
        assert_eq!(tnef_attachment_ref("tnef::1"), None);
        assert_eq!(tnef_attachment_ref("tnef:2:x"), None);
    }

    #[test]
    fn recognises_tnef_content_types() {
        assert!(is_tnef_content_type("application/ms-tnef"));
        assert!(is_tnef_content_type("Application/MS-TNEF"));
        assert!(is_tnef_content_type("application/x-tnef"));
        assert!(!is_tnef_content_type("application/pdf"));
    }

    #[test]
    fn guesses_content_types_from_extensions() {
        assert_eq!(content_type_for("report.pdf"), "application/pdf");
        assert_eq!(content_type_for("photo.JPG"), "image/jpeg");
        assert_eq!(content_type_for("no-extension"), "application/octet-stream");
    }

    #[test]
    fn mapi_named_properties_are_skipped_without_desync() {
        // A named property followed by a tagged one: the walk must skip the
        // GUID + name and still find the tagged property's value.
        let mut stream = Vec::new();
        stream.extend_from_slice(&2u32.to_le_bytes());
        // Named property: type binary, id 0x8005, GUID, kind = 0, id.
        stream.extend_from_slice(&MAPI_TYPE_BINARY.to_le_bytes());
        stream.extend_from_slice(&0x8005u16.to_le_bytes());
        stream.extend_from_slice(&[0u8; 16]);
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&0x1234u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&4u32.to_le_bytes());
        stream.extend_from_slice(b"skip");
        // Tagged property: PR_BODY as a unicode string.
        stream.extend_from_slice(&MAPI_TYPE_UNICODE_STRING.to_le_bytes());
        stream.extend_from_slice(&MAPI_BODY.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        let body = utf16("Found");
        stream.extend_from_slice(&(body.len() as u32).to_le_bytes());
        stream.extend_from_slice(&body);

        let properties = mapi_properties(&stream);
        assert_eq!(mapi_body(&properties, MAPI_BODY, None).as_deref(), Some("Found"));
    }
}
