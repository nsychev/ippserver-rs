//! The attributes a conformant Printer reports.
//!
//! Between them, IPP/2.0 [PWG 5100.12] and IPP Everywhere [PWG 5100.14] pin
//! down a long list of Printer Description attributes that MUST be present,
//! several with constrained values. They are all built here, in one place, so
//! that what the Printer claims and what it does are written next to each
//! other.
//!
//! The lists double as the Printer's policy: `document-format-supported` is
//! what [`crate::Backend`] will be asked to print, `media-supported` is what a
//! Client may ask for, and `job-creation-attributes-supported` is exactly the
//! set of Job Template attributes [`validate_template`] will not complain
//! about.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use ipp::attribute::IppAttribute;
use ipp::model::{JobState, PrinterState};
use ipp::value::{BoundedString, IppValue};

use crate::job::{Job, Moment, WHICH_JOBS};
use crate::media;
use crate::{Operations, PrinterConfig};

/// IPP's out-of-band `unknown` value tag.
const TAG_UNKNOWN: u8 = 0x12;

/// The only media type offered; see [`media::TYPES`].
const MEDIA_TYPE: &str = "stationery";

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

/// Trims a string to the length IPP allows for the attribute type it belongs
/// to.
///
/// Every string in an IPP value has a maximum length, and `ipp` gives each
/// one its own type rather than taking a `String`. Almost nothing here can
/// be too long, but a Client picks `job-name` and a deployment picks
/// `printer-info`, so an over-long value is trimmed rather than refused.
/// `BoundedString::new_truncated` cuts at a byte index, which panics in the
/// middle of a multi-byte character, so the cut is moved to a character
/// boundary first.
fn bounded<const MAX: usize>(value: &str) -> BoundedString<MAX> {
    let mut end = value.len().min(MAX);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    BoundedString::new_truncated(&value[..end])
}

pub(crate) fn keyword(value: &str) -> IppValue {
    IppValue::Keyword(bounded(value))
}

/// A `1setOf keyword`. One value is still a set of one.
fn keywords<I: IntoIterator<Item = S>, S: AsRef<str>>(values: I) -> IppValue {
    IppValue::Array(
        values
            .into_iter()
            .map(|v| keyword(v.as_ref()))
            .collect::<Vec<_>>(),
    )
}

fn enums<I: IntoIterator<Item = i32>>(values: I) -> IppValue {
    IppValue::Array(values.into_iter().map(IppValue::Enum).collect::<Vec<_>>())
}

fn text(value: &str) -> IppValue {
    IppValue::TextWithoutLanguage(bounded::<1023>(value).into())
}

/// A `status-message`, which RFC 8011 section 4.1.6.2 caps at 255 octets —
/// shorter than the text values above.
pub(crate) fn status_message(value: &str) -> IppValue {
    IppValue::TextWithoutLanguage(bounded::<255>(value).into())
}

fn name(value: &str) -> IppValue {
    IppValue::NameWithoutLanguage(bounded(value))
}

fn uri(value: impl AsRef<str>) -> IppValue {
    IppValue::Uri(bounded(value.as_ref()))
}

fn mime(value: &str) -> IppValue {
    IppValue::MimeMediaType(bounded(value))
}

fn charset(value: &str) -> IppValue {
    IppValue::Charset(bounded(value))
}

fn language(value: &str) -> IppValue {
    IppValue::NaturalLanguage(bounded(value))
}

/// A `dateTime`, always expressed in UTC.
fn date_time(at: DateTime<Utc>) -> IppValue {
    IppValue::DateTime(at.into())
}

fn resolution(dpi: i32) -> IppValue {
    IppValue::Resolution {
        cross_feed: dpi,
        feed: dpi,
        // 3 is dots per inch.
        units: 3,
    }
}

fn collection<const N: usize>(members: [(&str, IppValue); N]) -> IppValue {
    IppValue::Collection(BTreeMap::from(members.map(|(k, v)| (bounded(k), v))))
}

/// The out-of-band `unknown` value, for an attribute the Printer supports but
/// has no value for.
fn unknown() -> IppValue {
    IppValue::Other {
        tag: TAG_UNKNOWN,
        data: Default::default(),
    }
}

pub(crate) fn attr(name: &str, value: IppValue) -> IppAttribute {
    IppAttribute::new(bounded::<255>(name), value)
}

// ---------------------------------------------------------------------------
// Printer attributes
// ---------------------------------------------------------------------------

/// Everything needed to describe the Printer at this instant.
pub struct PrinterContext<'a> {
    pub config: &'a PrinterConfig,
    /// The authority from the request's `Host` header, e.g. `printer:631`.
    /// IPP Everywhere section 8.4 wants generated URIs to use it.
    pub host: &'a str,
    pub uptime: i32,
    pub queued: usize,
    pub state: PrinterState,
    pub state_change: Moment,
    pub config_change: Moment,
}

impl PrinterContext<'_> {
    /// The `ipp://` URI Clients should use.
    pub fn printer_uri(&self) -> String {
        format!("ipp://{}{}", self.host, self.config.path)
    }

    /// The `http://` URI for the same resource, for icons and web pages.
    pub fn http_uri(&self, path: &str) -> String {
        format!("http://{}{}", self.host, path)
    }

    fn job_uri(&self, id: i32) -> String {
        format!("ipp://{}{}/{}", self.host, self.config.path, id)
    }
}

/// `media-col` members this Printer accepts and reports.
const MEDIA_COL_MEMBERS: &[&str] = &[
    "media-bottom-margin",
    "media-left-margin",
    "media-right-margin",
    "media-size",
    "media-source",
    "media-top-margin",
    "media-type",
];

/// Job Template attributes a Client may send.
///
/// IPP Everywhere Table 12 requires all of these; the Printer honours what it
/// can and passes the rest to the real printer downstream.
pub const JOB_CREATION_ATTRIBUTES: &[&str] = &[
    "copies",
    "finishings",
    "ipp-attribute-fidelity",
    "job-name",
    "media",
    "media-col",
    "multiple-document-handling",
    "orientation-requested",
    "output-bin",
    "overrides",
    "page-ranges",
    "print-color-mode",
    "print-content-optimize",
    "print-quality",
    "print-rendering-intent",
    "print-scaling",
    "printer-resolution",
    "sides",
];

/// Builds a `media-col` style collection for one sheet.
/// One `media-col` per sheet offered, for `media-col-database` and
/// `media-col-ready`.
fn media_cols(config: &PrinterConfig) -> IppValue {
    IppValue::Array(
        config
            .media
            .iter()
            .map(|size| media_col(size, MEDIA_TYPE))
            .collect(),
    )
}

fn media_col(size: &media::MediaSize, media_type: &str) -> IppValue {
    collection([
        (
            "media-size",
            collection([
                ("x-dimension", IppValue::Integer(size.width)),
                ("y-dimension", IppValue::Integer(size.height)),
            ]),
        ),
        ("media-bottom-margin", IppValue::Integer(media::MARGIN)),
        ("media-left-margin", IppValue::Integer(media::MARGIN)),
        ("media-right-margin", IppValue::Integer(media::MARGIN)),
        ("media-top-margin", IppValue::Integer(media::MARGIN)),
        ("media-type", keyword(media_type)),
        ("media-source", keyword("auto")),
    ])
}

/// Every Printer Description attribute this Printer reports.
pub fn printer_attributes(ctx: &PrinterContext<'_>) -> Vec<IppAttribute> {
    let config = ctx.config;
    let printer_uri = ctx.printer_uri();

    let mut out = vec![
        // --- RFC 8011 required set ---
        attr("charset-configured", charset("utf-8")),
        attr("charset-supported", charset("utf-8")),
        attr(
            "compression-supported",
            keywords(["none", "deflate", "gzip"]),
        ),
        attr(
            "document-format-default",
            mime(&config.default_document_format),
        ),
        attr(
            "document-format-supported",
            IppValue::Array(config.document_formats.iter().map(|f| mime(f)).collect()),
        ),
        attr("generated-natural-language-supported", language("en")),
        attr("ipp-versions-supported", keywords(["1.0", "1.1", "2.0"])),
        attr("natural-language-configured", language("en")),
        attr(
            "operations-supported",
            enums(Operations::SUPPORTED.iter().map(|o| *o as i32)),
        ),
        attr("pdl-override-supported", keyword("attempted")),
        attr(
            "printer-is-accepting-jobs",
            IppValue::Boolean(config.accepting_jobs),
        ),
        attr("printer-name", name(&config.name)),
        attr("printer-state", IppValue::Enum(ctx.state as i32)),
        attr("printer-state-reasons", keywords(["none"])),
        attr("printer-up-time", IppValue::Integer(ctx.uptime)),
        attr(
            "printer-uri-supported",
            IppValue::Array(vec![uri(&printer_uri)]),
        ),
        attr("queued-job-count", IppValue::Integer(ctx.queued as i32)),
        // These three must have matching counts.
        attr("uri-authentication-supported", keywords(["none"])),
        attr("uri-security-supported", keywords(["none"])),
        // --- IPP/2.0 section 6.2 ---
        attr("color-supported", IppValue::Boolean(config.color)),
        attr("copies-default", IppValue::Integer(1)),
        attr(
            "copies-supported",
            IppValue::RangeOfInteger {
                min: 1,
                max: config.max_copies,
            },
        ),
        // 3 is 'none'; this Printer adds no finishing of its own.
        attr("finishings-default", IppValue::Enum(3)),
        attr("finishings-supported", enums([3])),
        // Every size offered is one more thing a Client can pick, so a
        // backend should claim only what it can really print on.
        attr("media-default", keyword(config.default_media.name)),
        attr("media-ready", keywords(config.media.iter().map(|m| m.name))),
        attr(
            "media-supported",
            keywords(config.media.iter().map(|m| m.name)),
        ),
        // 3 portrait, 4 landscape, 5 reverse-landscape, 6 reverse-portrait.
        attr("orientation-requested-default", IppValue::Enum(3)),
        attr("orientation-requested-supported", enums([3, 4, 5, 6])),
        attr("output-bin-default", keyword("face-down")),
        attr("output-bin-supported", keywords(["face-down"])),
        // 3 draft, 4 normal, 5 high.
        attr("print-quality-default", IppValue::Enum(4)),
        attr("print-quality-supported", enums([3, 4, 5])),
        attr(
            "printer-resolution-default",
            resolution(config.default_resolution),
        ),
        attr(
            "printer-resolution-supported",
            IppValue::Array(config.resolutions.iter().map(|d| resolution(*d)).collect()),
        ),
        attr("sides-default", keyword("one-sided")),
        attr(
            "sides-supported",
            keywords(["one-sided", "two-sided-long-edge", "two-sided-short-edge"]),
        ),
        attr(
            "pages-per-minute",
            IppValue::Integer(config.pages_per_minute),
        ),
        attr("printer-info", text(&config.info)),
        attr("printer-location", text(&config.location)),
        attr("printer-make-and-model", text(&config.make_and_model)),
        attr("printer-more-info", uri(ctx.http_uri("/"))),
        // --- IPP Everywhere Table 5 ---
        attr("identify-actions-default", keywords(["display"])),
        attr("identify-actions-supported", keywords(["display"])),
        attr(
            "ipp-features-supported",
            // A proxy in front of a real printer is a Logical Device, which
            // section 7.1 says MUST also report 'ipp-everywhere-server'.
            keywords(["ipp-everywhere", "ipp-everywhere-server"]),
        ),
        attr(
            "job-creation-attributes-supported",
            keywords(JOB_CREATION_ATTRIBUTES.iter().copied()),
        ),
        attr("job-ids-supported", IppValue::Boolean(true)),
        attr("preferred-attributes-supported", IppValue::Boolean(false)),
        attr(
            "media-bottom-margin-supported",
            IppValue::Array(vec![IppValue::Integer(media::MARGIN)]),
        ),
        attr(
            "media-left-margin-supported",
            IppValue::Array(vec![IppValue::Integer(media::MARGIN)]),
        ),
        attr(
            "media-right-margin-supported",
            IppValue::Array(vec![IppValue::Integer(media::MARGIN)]),
        ),
        attr(
            "media-top-margin-supported",
            IppValue::Array(vec![IppValue::Integer(media::MARGIN)]),
        ),
        attr(
            "media-col-supported",
            keywords(MEDIA_COL_MEMBERS.iter().copied()),
        ),
        attr(
            "media-col-default",
            media_col(&config.default_media, MEDIA_TYPE),
        ),
        attr("media-col-database", media_cols(config)),
        attr("media-col-ready", media_cols(config)),
        attr(
            "media-size-supported",
            IppValue::Array(
                config
                    .media
                    .iter()
                    .map(|size| {
                        collection([
                            ("x-dimension", IppValue::Integer(size.width)),
                            ("y-dimension", IppValue::Integer(size.height)),
                        ])
                    })
                    .collect(),
            ),
        ),
        attr(
            "media-source-supported",
            keywords(media::SOURCES.iter().copied()),
        ),
        attr(
            "media-type-supported",
            keywords(media::TYPES.iter().copied()),
        ),
        attr("multiple-document-jobs-supported", IppValue::Boolean(false)),
        attr("multiple-operation-time-out", IppValue::Integer(60)),
        attr("multiple-operation-time-out-action", keyword("abort-job")),
        attr(
            "overrides-supported",
            keywords(["document-number", "pages"]),
        ),
        attr(
            "print-color-mode-default",
            keyword(config.default_color_mode()),
        ),
        attr("print-color-mode-supported", keywords(config.color_modes())),
        attr("print-content-optimize-default", keyword("auto")),
        attr(
            "print-content-optimize-supported",
            keywords(["auto", "graphic", "photo", "text", "text-and-graphic"]),
        ),
        attr("print-rendering-intent-default", keyword("auto")),
        attr(
            "print-rendering-intent-supported",
            keywords([
                "auto",
                "perceptual",
                "relative",
                "relative-bpc",
                "saturation",
            ]),
        ),
        attr("print-scaling-default", keyword("auto")),
        attr(
            "print-scaling-supported",
            keywords(["auto", "auto-fit", "fill", "fit", "none"]),
        ),
        attr(
            "printer-config-change-date-time",
            date_time(ctx.config_change.at),
        ),
        attr(
            "printer-config-change-time",
            IppValue::Integer(ctx.config_change.ticks),
        ),
        attr("printer-device-id", text(&config.device_id())),
        attr(
            "printer-geo-location",
            config.geo_location.as_ref().map_or_else(unknown, uri),
        ),
        attr(
            "printer-get-attributes-supported",
            keywords(["document-format"]),
        ),
        attr(
            "printer-icons",
            IppValue::Array(
                crate::icons::ICONS
                    .iter()
                    .map(|icon| uri(ctx.http_uri(icon.path)))
                    .collect(),
            ),
        ),
        attr("printer-organization", text(&config.organization)),
        attr(
            "printer-organizational-unit",
            text(&config.organizational_unit),
        ),
        attr(
            "printer-state-change-date-time",
            date_time(ctx.state_change.at),
        ),
        attr(
            "printer-state-change-time",
            IppValue::Integer(ctx.state_change.ticks),
        ),
        attr("printer-state-message", text(config.state_message())),
        // A proxy has no consumables of its own, but the attribute pair is
        // required and their counts must match, so one placeholder entry
        // describes the situation honestly.
        attr(
            "printer-supply",
            IppValue::Array(vec![IppValue::OctetString(
                "index=1;class=receptacleThatIsFilled;type=notUsed;unit=percent;\
                 maxcapacity=100;level=-2;colorantindex=0;"
                    .into(),
            )]),
        ),
        attr(
            "printer-supply-description",
            IppValue::Array(vec![text(
                "Supplies are reported by the downstream printer",
            )]),
        ),
        attr("printer-supply-info-uri", uri(ctx.http_uri("/"))),
        attr("printer-uuid", uri(format!("urn:uuid:{}", config.uuid))),
        attr(
            "pwg-raster-document-resolution-supported",
            IppValue::Array(config.resolutions.iter().map(|d| resolution(*d)).collect()),
        ),
        // 'normal' means the Client sends back sides the same way up as
        // front sides; anything else would ask Clients to pre-transform
        // raster data for a sheet path this Printer does not have.
        attr("pwg-raster-document-sheet-back", keyword("normal")),
        attr(
            "pwg-raster-document-type-supported",
            keywords(config.advertised_raster_types()),
        ),
        attr("which-jobs-supported", keywords(WHICH_JOBS.iter().copied())),
        // 0 means the Printer supports the attribute but takes no passwords.
        attr("document-password-supported", IppValue::Integer(0)),
        attr(
            "job-impressions-supported",
            IppValue::RangeOfInteger {
                min: 0,
                max: config.max_pages_per_job.unwrap_or(i32::MAX),
            },
        ),
    ];

    // Only a colour Printer reports a colour rate; ipp-2.0.test checks that a
    // monochrome one does not.
    if config.color {
        out.push(attr(
            "pages-per-minute-color",
            IppValue::Integer(config.pages_per_minute),
        ));
    }
    // Conditionally required once "application/pdf" is accepted.
    if config.supports_pdf() {
        out.push(attr("page-ranges-supported", IppValue::Boolean(true)));
    }

    out
}

// ---------------------------------------------------------------------------
// Job attributes
// ---------------------------------------------------------------------------

/// The Job Description and Job Status attributes for one job.
pub fn job_attributes(ctx: &PrinterContext<'_>, job: &Job, intervening: i32) -> Vec<IppAttribute> {
    let mut out = vec![
        attr("job-id", IppValue::Integer(job.id)),
        attr("job-uri", uri(ctx.job_uri(job.id))),
        attr("job-printer-uri", uri(ctx.printer_uri())),
        attr("job-state", IppValue::Enum(job.state as i32)),
        attr(
            "job-state-reasons",
            keywords(job.reasons.iter().map(String::as_str)),
        ),
        attr("job-state-message", text(&job.message)),
        attr("job-name", name(&job.name)),
        attr("job-originating-user-name", name(&job.user)),
        attr("job-uuid", uri(format!("urn:uuid:{}", job.uuid))),
        attr("job-impressions", IppValue::Integer(job.impressions)),
        attr(
            "job-impressions-completed",
            IppValue::Integer(job.impressions_completed),
        ),
        attr("job-k-octets", IppValue::Integer(job.k_octets)),
        attr("job-printer-up-time", IppValue::Integer(ctx.uptime)),
        attr("number-of-intervening-jobs", IppValue::Integer(intervening)),
        attr("time-at-creation", IppValue::Integer(job.created.ticks)),
        attr("date-time-at-creation", date_time(job.created.at)),
        attr("document-format", mime(&job.document_format)),
    ];

    // A job that has not reached a stage reports no-value for it rather than
    // a misleading zero.
    match job.processing {
        Some(m) => {
            out.push(attr("time-at-processing", IppValue::Integer(m.ticks)));
            out.push(attr("date-time-at-processing", date_time(m.at)));
        }
        None => {
            out.push(attr("time-at-processing", IppValue::NoValue));
            out.push(attr("date-time-at-processing", IppValue::NoValue));
        }
    }
    match job.completed {
        Some(m) => {
            out.push(attr("time-at-completed", IppValue::Integer(m.ticks)));
            out.push(attr("date-time-at-completed", date_time(m.at)));
        }
        None => {
            out.push(attr("time-at-completed", IppValue::NoValue));
            out.push(attr("date-time-at-completed", IppValue::NoValue));
        }
    }

    // Whatever Job Template attributes the Client sent and we accepted.
    out.extend(job.template.iter().cloned());
    out
}

/// The `job-state-reasons` keyword that goes with a final state.
pub fn reason_for(state: JobState) -> &'static str {
    match state {
        JobState::Completed => "job-completed-successfully",
        JobState::Canceled => "job-canceled-by-user",
        JobState::Aborted => "job-aborted-by-system",
        JobState::Processing => "job-printing",
        JobState::Pending => "none",
        JobState::PendingHeld => "job-hold-until-specified",
        JobState::ProcessingStopped => "job-printing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, test_config};

    fn find<'a>(attrs: &'a [IppAttribute], name: &str) -> Option<&'a IppAttribute> {
        attrs.iter().find(|a| a.name().as_str() == name)
    }

    fn values(attrs: &[IppAttribute], name: &str) -> Vec<String> {
        match find(attrs, name).map(IppAttribute::value) {
            Some(IppValue::Array(items)) => items.iter().map(|v| v.to_string()).collect(),
            Some(other) => vec![other.to_string()],
            None => Vec::new(),
        }
    }

    #[test]
    fn every_attribute_ipp_everywhere_requires_is_present() {
        let config = test_config();
        let attrs = printer_attributes(&context(&config));

        // Table 5 of PWG 5100.14, less the ones that are conditional on
        // features this Printer does not offer (finishers, paid printing,
        // ICC colour management, held jobs).
        for name in [
            "charset-configured",
            "charset-supported",
            "color-supported",
            "compression-supported",
            "copies-default",
            "copies-supported",
            "document-format-default",
            "document-format-supported",
            "document-password-supported",
            "generated-natural-language-supported",
            "identify-actions-default",
            "identify-actions-supported",
            "ipp-features-supported",
            "ipp-versions-supported",
            "job-creation-attributes-supported",
            "job-ids-supported",
            "media-bottom-margin-supported",
            "media-col-database",
            "media-col-default",
            "media-col-ready",
            "media-col-supported",
            "media-default",
            "media-left-margin-supported",
            "media-ready",
            "media-right-margin-supported",
            "media-size-supported",
            "media-source-supported",
            "media-supported",
            "media-top-margin-supported",
            "media-type-supported",
            "multiple-document-jobs-supported",
            "multiple-operation-time-out",
            "multiple-operation-time-out-action",
            "natural-language-configured",
            "operations-supported",
            "orientation-requested-default",
            "orientation-requested-supported",
            "output-bin-default",
            "output-bin-supported",
            "overrides-supported",
            "page-ranges-supported",
            "pdl-override-supported",
            "preferred-attributes-supported",
            "print-color-mode-default",
            "print-color-mode-supported",
            "print-quality-default",
            "print-quality-supported",
            "print-rendering-intent-default",
            "print-rendering-intent-supported",
            "printer-config-change-date-time",
            "printer-config-change-time",
            "printer-device-id",
            "printer-geo-location",
            "printer-get-attributes-supported",
            "printer-icons",
            "printer-info",
            "printer-is-accepting-jobs",
            "printer-location",
            "printer-make-and-model",
            "printer-name",
            "printer-organization",
            "printer-organizational-unit",
            "printer-resolution-default",
            "printer-resolution-supported",
            "printer-state",
            "printer-state-change-date-time",
            "printer-state-change-time",
            "printer-state-reasons",
            "printer-supply",
            "printer-supply-description",
            "printer-supply-info-uri",
            "printer-up-time",
            "printer-uri-supported",
            "printer-uuid",
            "pwg-raster-document-resolution-supported",
            "pwg-raster-document-sheet-back",
            "pwg-raster-document-type-supported",
            "queued-job-count",
            "sides-default",
            "sides-supported",
            "uri-authentication-supported",
            "uri-security-supported",
            "which-jobs-supported",
        ] {
            assert!(find(&attrs, name).is_some(), "missing {name}");
        }
    }

    #[test]
    fn the_required_document_formats_are_advertised() {
        let config = test_config();
        let attrs = printer_attributes(&context(&config));
        let formats = values(&attrs, "document-format-supported");
        // PWG Raster is the one IPP Everywhere actually requires.
        assert!(
            formats.iter().any(|f| f == "image/pwg-raster"),
            "{formats:?}"
        );
        assert!(formats.iter().any(|f| f == "image/jpeg"), "{formats:?}");
        assert!(
            formats.iter().any(|f| f == "application/pdf"),
            "{formats:?}"
        );
    }

    #[test]
    fn the_printer_announces_itself_as_an_ipp_everywhere_server() {
        let config = test_config();
        let attrs = printer_attributes(&context(&config));
        let features = values(&attrs, "ipp-features-supported");
        assert!(features.iter().any(|f| f == "ipp-everywhere"));
        // It proxies to a real printer, so it is a Logical Device.
        assert!(features.iter().any(|f| f == "ipp-everywhere-server"));
    }

    #[test]
    fn uris_are_absolute_and_built_from_the_host_header() {
        let config = test_config();
        let ctx = PrinterContext {
            host: "printer.example:6631",
            ..context(&config)
        };
        let attrs = printer_attributes(&ctx);

        assert_eq!(
            values(&attrs, "printer-uri-supported"),
            vec!["ipp://printer.example:6631/ipp/print"]
        );
        for icon in values(&attrs, "printer-icons") {
            assert!(icon.starts_with("http://printer.example:6631/"), "{icon}");
        }
        assert!(values(&attrs, "printer-more-info")[0].starts_with("http://printer.example:6631/"));
    }

    #[test]
    fn the_security_keyword_counts_match_the_uri_count() {
        let config = test_config();
        let attrs = printer_attributes(&context(&config));
        let uris = values(&attrs, "printer-uri-supported").len();
        assert_eq!(values(&attrs, "uri-security-supported").len(), uris);
        assert_eq!(values(&attrs, "uri-authentication-supported").len(), uris);
    }

    #[test]
    fn the_supply_attributes_have_matching_counts() {
        let config = test_config();
        let attrs = printer_attributes(&context(&config));
        assert_eq!(
            values(&attrs, "printer-supply").len(),
            values(&attrs, "printer-supply-description").len()
        );
    }

    #[test]
    fn a_monochrome_printer_does_not_claim_a_colour_rate() {
        let mut config = test_config();
        config.color = false;
        let attrs = printer_attributes(&context(&config));
        assert!(find(&attrs, "pages-per-minute-color").is_none());
        assert!(!values(&attrs, "print-color-mode-supported").contains(&"color".to_string()));

        config.color = true;
        let attrs = printer_attributes(&context(&config));
        assert!(find(&attrs, "pages-per-minute-color").is_some());
    }

    #[test]
    fn page_ranges_are_only_claimed_alongside_pdf() {
        let mut config = test_config();
        config.document_formats = vec!["image/pwg-raster".into()];
        config.default_document_format = "image/pwg-raster".into();
        let attrs = printer_attributes(&context(&config));
        assert!(find(&attrs, "page-ranges-supported").is_none());
    }

    #[test]
    fn every_sheet_offered_appears_in_each_media_list() {
        let mut config = test_config();
        config.media = vec![
            *media::find("na_letter_8.5x11in").unwrap(),
            *media::find("iso_a4_210x297mm").unwrap(),
        ];
        config.default_media = *media::find("na_letter_8.5x11in").unwrap();
        let attrs = printer_attributes(&context(&config));

        // Supported and ready carry every sheet, in the order configured.
        for name in ["media-supported", "media-ready"] {
            assert_eq!(
                values(&attrs, name),
                vec!["na_letter_8.5x11in", "iso_a4_210x297mm"],
                "{name}"
            );
        }
        // The default is a single value: the first sheet offered.
        assert_eq!(values(&attrs, "media-default"), vec!["na_letter_8.5x11in"]);

        // The collection forms carry one entry per sheet, each with a size
        // and four margins.
        for name in ["media-col-database", "media-col-ready"] {
            let IppValue::Array(entries) = find(&attrs, name).unwrap().value() else {
                panic!("{name} is not a set");
            };
            assert_eq!(entries.len(), 2, "{name}");
            let IppValue::Collection(entry) = &entries[0] else {
                panic!("{name} entry is not a collection");
            };
            assert!(entry.contains_key("media-size"));
            assert!(entry.contains_key("media-top-margin"));
            let Some(IppValue::Collection(size)) = entry.get("media-size") else {
                panic!("media-size is not a collection");
            };
            // And the dimensions are the ones the first name describes.
            assert_eq!(size.get("x-dimension"), Some(&IppValue::Integer(21590)));
            assert_eq!(size.get("y-dimension"), Some(&IppValue::Integer(27940)));
        }

        // As does media-size-supported, one collection per sheet.
        let IppValue::Array(sizes) = find(&attrs, "media-size-supported").unwrap().value() else {
            panic!("media-size-supported is not a set");
        };
        assert_eq!(sizes.len(), 2);
    }

    #[test]
    fn the_default_media_is_one_of_the_supported_ones() {
        let config = test_config();
        let attrs = printer_attributes(&context(&config));
        let default = &values(&attrs, "media-default")[0];
        assert!(values(&attrs, "media-supported").contains(default));
    }

    #[test]
    fn the_device_id_matches_the_shape_clients_parse() {
        let config = test_config();
        let attrs = printer_attributes(&context(&config));
        let id = values(&attrs, "printer-device-id").remove(0);
        assert!(id.ends_with(';'), "{id}");
        for field in id.trim_end_matches(';').split(';') {
            let (key, _) = field.split_once(':').expect(&id);
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '-'),
                "{id}"
            );
        }
    }

    #[test]
    fn a_job_reports_no_value_for_stages_it_has_not_reached() {
        use crate::test_support::pending_job;

        let config = test_config();
        let ctx = context(&config);
        let job = pending_job();
        let attrs = job_attributes(&ctx, &job, 0);

        assert!(matches!(
            find(&attrs, "time-at-processing").unwrap().value(),
            IppValue::NoValue
        ));
        assert!(matches!(
            find(&attrs, "time-at-creation").unwrap().value(),
            IppValue::Integer(_)
        ));
        // The required Job Status attributes are all there.
        for name in [
            "job-id",
            "job-uri",
            "job-printer-uri",
            "job-state",
            "job-state-reasons",
            "job-state-message",
            "job-name",
            "job-originating-user-name",
            "job-uuid",
            "job-impressions",
            "job-impressions-completed",
            "job-printer-up-time",
            "time-at-creation",
            "date-time-at-creation",
            "time-at-completed",
        ] {
            assert!(find(&attrs, name).is_some(), "missing {name}");
        }
    }

    #[test]
    fn job_uuids_and_uris_are_absolute_urns() {
        use crate::test_support::pending_job;

        let config = test_config();
        let ctx = context(&config);
        let attrs = job_attributes(&ctx, &pending_job(), 0);
        let uuid = values(&attrs, "job-uuid").remove(0);
        assert!(uuid.starts_with("urn:uuid:"), "{uuid}");
        let uri = values(&attrs, "job-uri").remove(0);
        assert!(uri.starts_with("ipp://"), "{uri}");
    }
}
