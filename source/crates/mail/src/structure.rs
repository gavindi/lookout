//! Flattens an IMAP `BODYSTRUCTURE` response into the flat leaf-part list the
//! rest of the crate works with (`BodyPart`). This is the metadata that makes
//! the viewer's partial-fetch path possible: part numbers are IMAP `BODY[<n>]`
//! section paths, so "fetch only the text parts" is a simple filter over the
//! flattened list, and attachment parts are never downloaded at all.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use async_imap::imap_proto::types::{BodyContentCommon, BodyContentSinglePart, BodyStructure, ContentEncoding};
use lookout_core::BodyPart;

/// Flattens a `BODYSTRUCTURE` response (as returned by `Fetch::bodystructure`)
/// into the message's leaf parts in document order, with `BODY[<part>]`-style
/// part numbers (RFC 3501 §6.4.5: `multipart` children number `1..N`, and a
/// `message/rfc822` part's enclosed message is *not* descended into - an
/// attached email is an attachment, not body text).
pub fn parts_from_bodystructure(structure: &BodyStructure) -> Vec<BodyPart> {
    let mut parts = Vec::new();
    walk(structure, &mut Vec::new(), &mut parts);
    parts
}

/// Whether the flattened structure contains any attachment part - the source
/// of truth for `EmailSummary::has_attachment`.
pub fn has_attachments(parts: &[BodyPart]) -> bool {
    parts.iter().any(|p| p.is_attachment)
}

/// Whether the flattened structure contains an iCalendar (`text/calendar`)
/// part - the source of truth for `EmailSummary::has_calendar`, and the
/// summary-level counterpart of the reading pane's invitation banner.
/// Independent of `has_attachments`: an invite that declares a filename
/// (e.g. `invite.ics`) counts as both, one without does not count as an
/// attachment.
pub fn has_calendar_parts(parts: &[BodyPart]) -> bool {
    parts.iter().any(|p| p.is_calendar())
}

fn walk(structure: &BodyStructure, path: &mut Vec<u32>, out: &mut Vec<BodyPart>) {
    match structure {
        BodyStructure::Multipart { bodies, .. } => {
            for (i, child) in bodies.iter().enumerate() {
                path.push(i as u32 + 1);
                walk(child, path, out);
                path.pop();
            }
        }
        BodyStructure::Message { common, other, .. } => push_leaf(common, other, path, out, true),
        BodyStructure::Basic { common, other, .. } => push_leaf(common, other, path, out, false),
        BodyStructure::Text { common, other, .. } => push_leaf(common, other, path, out, false),
    }
}

fn push_leaf<'a>(common: &'a BodyContentCommon<'a>, other: &'a BodyContentSinglePart<'a>, path: &mut Vec<u32>, out: &mut Vec<BodyPart>, embedded_message: bool) {
    // A top-level single-part message is part `1` (RFC 3501: `BODY[1]` is
    // the whole part), while a leaf inside a multipart carries its parent
    // path as built by `walk`.
    let root = path.is_empty();
    if root {
        path.push(1);
    }
    out.push(leaf(common, other, path, embedded_message));
    if root {
        path.pop();
    }
}

fn leaf(common: &BodyContentCommon, other: &BodyContentSinglePart, path: &[u32], embedded_message: bool) -> BodyPart {
    let ty = &common.ty.ty;
    let subtype = &common.ty.subtype;

    // Content-Type parameters (`charset`, `name`, ...) and disposition
    // parameters (`filename`), matched case-insensitively like every other
    // header field.
    let param = |key: &str| {
        common
            .ty
            .params
            .as_ref()
            .and_then(|params| params.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_ref()))
    };
    let disposition_param = |key: &str| {
        common
            .disposition
            .as_ref()
            .and_then(|d| d.params.as_ref())
            .and_then(|params| params.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_ref()))
    };

    let filename = disposition_param("filename").or_else(|| param("name")).map(str::to_string);

    // A text/plain or text/html leaf is the body, never an attachment - even
    // when it declares `Content-Disposition: attachment` (a quirk some
    // servers emit for the plain-text half of an alternative). An attachment
    // is any other non-text leaf that declares `Content-Disposition:
    // attachment`, or carries a filename without an explicit `inline`
    // disposition. Inline `cid:` images inside `multipart/related` stay
    // non-attachments even when they carry a filename (senders commonly
    // attach `filename=` to an inline image), so they never surface in the
    // attachment strip. An embedded `message/rfc822` is always an attachment.
    let is_text_leaf = ty.eq_ignore_ascii_case("text") && (subtype.eq_ignore_ascii_case("plain") || subtype.eq_ignore_ascii_case("html"));
    let disposition_is = |ty: &str| matches!(&common.disposition, Some(d) if d.ty.eq_ignore_ascii_case(ty));
    let is_attachment = embedded_message || !is_text_leaf && (disposition_is("attachment") || filename.is_some() && !disposition_is("inline"));

    BodyPart {
        part_number: path.iter().map(u32::to_string).collect::<Vec<_>>().join("."),
        content_type: format!("{ty}/{subtype}").to_ascii_lowercase(),
        charset: param("charset").map(str::to_string),
        transfer_encoding: Some(encoding_str(&other.transfer_encoding).to_string()),
        filename,
        cid: other.id.as_ref().map(|s| s.to_string()),
        size: other.octets,
        is_attachment,
    }
}

fn encoding_str<'a>(encoding: &'a ContentEncoding<'a>) -> &'a str {
    match encoding {
        ContentEncoding::SevenBit => "7bit",
        ContentEncoding::EightBit => "8bit",
        ContentEncoding::Binary => "binary",
        ContentEncoding::Base64 => "base64",
        ContentEncoding::QuotedPrintable => "quoted-printable",
        ContentEncoding::Other(s) => s.as_ref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_imap::imap_proto::types::ContentDisposition;

    /// A disposition type plus optional disposition params, mirroring
    /// `ContentDisposition`'s shape for the `common` helper below.
    type Disposition<'a> = Option<(&'a str, Option<Vec<(&'a str, &'a str)>>)>;

    fn content_type<'a>(ty: &'a str, subtype: &'a str, params: Option<Vec<(&'a str, &'a str)>>) -> async_imap::imap_proto::types::ContentType<'a> {
        async_imap::imap_proto::types::ContentType {
            ty: ty.into(),
            subtype: subtype.into(),
            params: params.map(|ps| ps.into_iter().map(|(k, v)| (k.into(), v.into())).collect()),
        }
    }

    fn common<'a>(ty: &'a str, subtype: &'a str, disposition: Disposition<'a>, params: Option<Vec<(&'a str, &'a str)>>) -> BodyContentCommon<'a> {
        BodyContentCommon {
            ty: content_type(ty, subtype, params),
            disposition: disposition.map(|(ty, params)| ContentDisposition {
                ty: ty.into(),
                params: params.map(|ps| ps.into_iter().map(|(k, v)| (k.into(), v.into())).collect()),
            }),
            language: None,
            location: None,
        }
    }

    fn single<'a>(id: Option<&'a str>, encoding: ContentEncoding<'a>, octets: u32) -> BodyContentSinglePart<'a> {
        BodyContentSinglePart {
            id: id.map(Into::into),
            md5: None,
            description: None,
            transfer_encoding: encoding,
            octets,
        }
    }

    fn text_plain() -> BodyStructure<'static> {
        BodyStructure::Text {
            common: common("text", "plain", None, Some(vec![("charset", "utf-8")])),
            other: single(None, ContentEncoding::Base64, 42),
            lines: 3,
            extension: None,
        }
    }

    #[test]
    fn single_plain_text_part_numbers_as_one() {
        let parts = parts_from_bodystructure(&text_plain());
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_number, "1");
        assert_eq!(parts[0].content_type, "text/plain");
        assert_eq!(parts[0].charset.as_deref(), Some("utf-8"));
        assert_eq!(parts[0].transfer_encoding.as_deref(), Some("base64"));
        assert_eq!(parts[0].size, 42);
        assert!(parts[0].is_text());
        assert!(!parts[0].is_attachment);
        assert!(!has_attachments(&parts));
    }

    #[test]
    fn text_part_never_counts_as_an_attachment() {
        // Even an explicit `Content-Disposition: attachment` on a text part
        // doesn't make it one - it's still the body.
        let bs = BodyStructure::Text {
            common: common("text", "plain", Some(("attachment", None)), Some(vec![("charset", "utf-8")])),
            other: single(None, ContentEncoding::SevenBit, 10),
            lines: 1,
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert!(!parts[0].is_attachment);
    }

    #[test]
    fn mixed_message_numbers_children_and_marks_real_attachments() {
        let bs = BodyStructure::Multipart {
            common: common("multipart", "mixed", None, None),
            bodies: vec![
                text_plain(),
                BodyStructure::Basic {
                    common: common("application", "pdf", Some(("attachment", Some(vec![("filename", "report.pdf")]))), None),
                    other: single(None, ContentEncoding::Base64, 4096),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].part_number, "1");
        assert_eq!(parts[0].content_type, "text/plain");
        assert!(!parts[0].is_attachment);
        assert_eq!(parts[1].part_number, "2");
        assert_eq!(parts[1].content_type, "application/pdf");
        assert_eq!(parts[1].filename.as_deref(), Some("report.pdf"));
        assert_eq!(parts[1].size, 4096);
        assert!(parts[1].is_attachment);
        assert!(has_attachments(&parts));
    }

    #[test]
    fn alternative_children_get_dotted_part_numbers() {
        let bs = BodyStructure::Multipart {
            common: common("multipart", "alternative", None, None),
            bodies: vec![
                text_plain(),
                BodyStructure::Text {
                    common: common("text", "html", None, Some(vec![("charset", "utf-8")])),
                    other: single(None, ContentEncoding::QuotedPrintable, 500),
                    lines: 10,
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].part_number, "1");
        assert_eq!(parts[1].part_number, "2");
        assert_eq!(parts[1].content_type, "text/html");
        assert_eq!(parts[1].transfer_encoding.as_deref(), Some("quoted-printable"));
        assert!(parts[1].is_text());
        assert!(!has_attachments(&parts));
    }

    #[test]
    fn inline_image_without_a_name_is_not_an_attachment() {
        // multipart/related [ text/html, image/png (inline, cid, no name) ].
        let bs = BodyStructure::Multipart {
            common: common("multipart", "related", None, None),
            bodies: vec![
                BodyStructure::Text {
                    common: common("text", "html", None, Some(vec![("charset", "utf-8")])),
                    other: single(None, ContentEncoding::SevenBit, 300),
                    lines: 6,
                    extension: None,
                },
                BodyStructure::Basic {
                    common: common("image", "png", Some(("inline", None)), None),
                    other: single(Some("logo123@lookout.test"), ContentEncoding::Base64, 1024),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].part_number, "1");
        assert_eq!(parts[1].part_number, "2");
        assert_eq!(parts[1].cid.as_deref(), Some("logo123@lookout.test"));
        assert!(!parts[1].is_attachment);
        assert!(!has_attachments(&parts));
    }

    #[test]
    fn inline_image_with_a_filename_is_not_an_attachment() {
        // Senders routinely attach `filename=` to an inline image; the
        // disposition - not the filename - decides whether it belongs in the
        // attachment strip.
        let bs = BodyStructure::Multipart {
            common: common("multipart", "related", None, None),
            bodies: vec![
                BodyStructure::Text {
                    common: common("text", "html", None, Some(vec![("charset", "utf-8")])),
                    other: single(None, ContentEncoding::SevenBit, 300),
                    lines: 6,
                    extension: None,
                },
                BodyStructure::Basic {
                    common: common("image", "png", Some(("inline", Some(vec![("filename", "logo.png")]))), None),
                    other: single(Some("logo123"), ContentEncoding::Base64, 1024),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert_eq!(parts[1].filename.as_deref(), Some("logo.png"));
        assert!(!parts[1].is_attachment);
        assert!(!has_attachments(&parts));
    }

    #[test]
    fn filename_without_a_disposition_is_an_attachment() {
        // A named part with no disposition at all is still an attachment.
        let bs = BodyStructure::Multipart {
            common: common("multipart", "mixed", None, None),
            bodies: vec![
                text_plain(),
                BodyStructure::Basic {
                    common: common("application", "pdf", None, Some(vec![("name", "report.pdf")])),
                    other: single(None, ContentEncoding::Base64, 4096),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert_eq!(parts[1].filename.as_deref(), Some("report.pdf"));
        assert!(parts[1].is_attachment);
        assert!(has_attachments(&parts));
    }

    #[test]
    fn embedded_message_is_an_attachment_and_is_not_descended_into() {
        let bs = BodyStructure::Message {
            common: common("message", "rfc822", Some(("attachment", Some(vec![("filename", "fwd.eml")]))), None),
            other: single(None, ContentEncoding::SevenBit, 2048),
            envelope: async_imap::imap_proto::types::Envelope {
                date: None,
                subject: None,
                from: None,
                sender: None,
                reply_to: None,
                to: None,
                cc: None,
                bcc: None,
                in_reply_to: None,
                message_id: None,
            },
            // The enclosed message would carry text parts numbered 1.1, 1.2 -
            // they must not surface: an attached email is an attachment.
            body: Box::new(BodyStructure::Multipart {
                common: common("multipart", "alternative", None, None),
                bodies: vec![text_plain()],
                extension: None,
            }),
            lines: 4,
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_number, "1");
        assert_eq!(parts[0].content_type, "message/rfc822");
        assert_eq!(parts[0].filename.as_deref(), Some("fwd.eml"));
        assert!(parts[0].is_attachment);
        assert!(!parts[0].is_text());
        assert!(has_attachments(&parts));
    }

    #[test]
    fn calendar_part_is_detected_with_and_without_a_filename() {
        // An iMIP invitation without a filename (the common Outlook form) -
        // calendar yes, attachment no.
        let unnamed = BodyStructure::Multipart {
            common: common("multipart", "mixed", None, None),
            bodies: vec![
                text_plain(),
                BodyStructure::Basic {
                    common: common("text", "calendar", None, None),
                    other: single(None, ContentEncoding::QuotedPrintable, 2000),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&unnamed);
        assert!(parts[1].is_calendar());
        assert!(!parts[1].is_attachment);
        assert!(has_calendar_parts(&parts));
        assert!(!has_attachments(&parts));

        // A named invite (`invite.ics`) counts as both - the two indicators
        // are independent.
        let named = BodyStructure::Multipart {
            common: common("multipart", "mixed", None, None),
            bodies: vec![
                text_plain(),
                BodyStructure::Basic {
                    common: common("text", "calendar", Some(("attachment", Some(vec![("filename", "invite.ics")]))), None),
                    other: single(None, ContentEncoding::QuotedPrintable, 2000),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&named);
        assert!(has_calendar_parts(&parts));
        assert!(has_attachments(&parts));
    }

    #[test]
    fn ordinary_mail_has_no_calendar_parts() {
        let bs = BodyStructure::Multipart {
            common: common("multipart", "mixed", None, None),
            bodies: vec![
                text_plain(),
                BodyStructure::Basic {
                    common: common("application", "pdf", Some(("attachment", Some(vec![("filename", "report.pdf")]))), None),
                    other: single(None, ContentEncoding::Base64, 4096),
                    extension: None,
                },
            ],
            extension: None,
        };
        let parts = parts_from_bodystructure(&bs);
        assert!(has_attachments(&parts));
        assert!(!has_calendar_parts(&parts));
    }

    #[test]
    fn empty_multipart_yields_no_parts() {
        let bs = BodyStructure::Multipart {
            common: common("multipart", "mixed", None, None),
            bodies: vec![],
            extension: None,
        };
        assert!(parts_from_bodystructure(&bs).is_empty());
    }
}
