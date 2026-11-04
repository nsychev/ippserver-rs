//! An IPP/2.0 and IPP Everywhere™ conformant Printer service with driverless
//! printing support.
//!
//! # Usage
//!
//! Implement `Backend`. It has two methods `validate` and `print`:
//!
//! ```rust
//! use ippserver::{Accepted, Backend, JobRequest, Rejection};
//!
//! struct MyPrinter;
//!
//! #[async_trait::async_trait]
//! impl Backend for MyPrinter {
//!     type Plan = ();
//!
//!     /// Synchronous part: the client waits for a response, you may refuse the
//!     /// job immediately.
//!     async fn validate(&self, job: &JobRequest) -> Result<Accepted<()>, Rejection> {
//!         Ok(Accepted { impressions: 1, format: None, plan: () })
//!     }
//!
//!     /// Asynchronous part, runs on worker when the job is already queued.
//!     async fn print(&self, job: JobRequest, _plan: ()) -> Result<(), String> {
//!         println!("printing {} bytes", job.document.len());
//!         Ok(())
//!     }
//! }
//! ```
//!
//! Then serve it:
//!
//! ```rust,no_run
//! # use ippserver::*;
//! # async fn example<B: Backend>(backend: B) -> Result<(), Box<dyn std::error::Error>> {
//! // `default()` leaves the identifying fields blank, because there is no
//! // sensible default for someone else's printer.
//! let config = PrinterConfig {
//!     name: "Upstairs".into(),
//!     manufacturer: "Acme".into(),
//!     make_and_model: "Acme Jet 100".into(),
//!     ..PrinterConfig::default()
//! };
//! let printer = Printer::start(config, backend)?;
//! serve("0.0.0.0:631".parse().unwrap(), printer).await?;
//! # Ok(())
//! # }
//! ```

pub mod attributes;
pub mod discovery;
pub mod icons;
pub mod job;
pub mod media;

mod http;
mod service;

#[cfg(test)]
mod test_support;

pub use http::{router, serve};
pub use ipp::model::{JobState, PrinterState, StatusCode};
pub use service::{Printer, RequestContext};

use std::net::IpAddr;

use ipp::attribute::IppAttribute;
use ipp::value::IppValue;
use uuid::Uuid;

/// The IPP operations this Printer implements.
pub struct Operations;

impl Operations {
    pub const PRINT_JOB: u16 = 0x0002;
    pub const VALIDATE_JOB: u16 = 0x0004;
    pub const CREATE_JOB: u16 = 0x0005;
    pub const SEND_DOCUMENT: u16 = 0x0006;
    pub const CANCEL_JOB: u16 = 0x0008;
    pub const GET_JOB_ATTRIBUTES: u16 = 0x0009;
    pub const GET_JOBS: u16 = 0x000A;
    pub const GET_PRINTER_ATTRIBUTES: u16 = 0x000B;
    pub const CANCEL_MY_JOBS: u16 = 0x0039;
    pub const CLOSE_JOB: u16 = 0x003B;
    pub const IDENTIFY_PRINTER: u16 = 0x003C;

    pub const SUPPORTED: &'static [u16] = &[
        Self::PRINT_JOB,
        Self::VALIDATE_JOB,
        Self::CREATE_JOB,
        Self::SEND_DOCUMENT,
        Self::CANCEL_JOB,
        Self::GET_JOB_ATTRIBUTES,
        Self::GET_JOBS,
        Self::GET_PRINTER_ATTRIBUTES,
        Self::CANCEL_MY_JOBS,
        Self::CLOSE_JOB,
        Self::IDENTIFY_PRINTER,
    ];
}

/// Validation errors for [`PrinterConfig`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A required `1setOf` attribute would be advertised with no values.
    #[error("`{attribute}` would be empty, and IPP requires at least one value")]
    NothingOffered { attribute: &'static str },

    /// An `xxx-default` is not among the matching `xxx-supported`.
    #[error("`{attribute}` is `{value}`, which is not one of the values offered")]
    DefaultNotOffered {
        attribute: &'static str,
        value: String,
    },

    /// A field that identifies the Printer was left blank.
    #[error("`{field}` must not be blank")]
    Blank { field: &'static str },

    /// The resource path is not usable in the URIs the Printer hands out.
    #[error("the resource path `{0}` must start with `/`, e.g. `/ipp/print`")]
    BadPath(String),

    /// `copies-supported` would be an empty or inverted range.
    #[error("max_copies is {0}, and a Printer must accept at least one copy")]
    BadMaxCopies(i32),
}

/// Advertised Printer configuration.
#[derive(Debug, Clone)]
pub struct PrinterConfig {
    pub name: String,
    pub info: String,
    pub location: String,
    pub manufacturer: String,
    pub make_and_model: String,
    pub organization: String,
    pub organizational_unit: String,
    pub uuid: Uuid,
    /// The resource path, e.g. `/ipp/print`.
    pub path: String,
    /// MIME media types accepted, in `document-format-supported`.
    pub document_formats: Vec<String>,
    pub default_document_format: String,
    /// The sheet sizes offered, in `media-supported` and `media-ready`.
    pub media: Vec<media::MediaSize>,
    /// The sheet in `media-default`, which must be one of
    /// [`PrinterConfig::media`].
    pub default_media: media::MediaSize,
    /// Whether to advertise colour.
    pub color: bool,
    pub resolutions: Vec<i32>,
    pub default_resolution: i32,
    /// The raster flavours the backend can decode.
    pub raster_types: Vec<&'static str>,
    /// Not a limit, just announced value.
    pub pages_per_minute: i32,
    pub max_copies: i32,
    /// Refuse a job with more pages than this.
    pub max_pages_per_job: Option<i32>,
    /// Refuse a submission while this client already has this many jobs
    /// queued or printing.
    pub max_queued_per_origin: Option<usize>,
    /// Refuse any document larger than this.
    pub max_document_bytes: usize,
    /// How many finished jobs stay queryable.
    pub job_history: usize,
    pub accepting_jobs: bool,
    /// A `geo:` URI, if the Printer's location is known.
    pub geo_location: Option<String>,
    /// This machine's host name.
    ///
    /// PWG 5100.12 section 6.5 requires the `Host` header to be validated.
    pub hostname: Option<String>,
}

impl Default for PrinterConfig {
    fn default() -> Self {
        PrinterConfig {
            name: String::new(),
            info: String::new(),
            location: String::new(),
            manufacturer: String::new(),
            make_and_model: String::new(),
            organization: String::new(),
            organizational_unit: String::new(),
            uuid: Uuid::nil(),
            path: "/ipp/print".into(),
            document_formats: vec![
                "application/pdf".into(),
                "image/jpeg".into(),
                "image/pwg-raster".into(),
            ],
            default_document_format: "image/pwg-raster".into(),
            media: vec![*media::default_size()],
            default_media: *media::default_size(),
            color: false,
            resolutions: vec![150, 300, 600],
            default_resolution: 300,
            raster_types: vec![
                "black_1",
                "sgray_1",
                "black_8",
                "sgray_8",
                "srgb_8",
                "cmyk_8",
                "adobe-rgb_8",
                "black_16",
                "sgray_16",
                "srgb_16",
                "cmyk_16",
                "adobe-rgb_16",
            ],
            pages_per_minute: 20,
            max_copies: 99,
            max_pages_per_job: None,
            max_queued_per_origin: None,
            max_document_bytes: 50 * 1024 * 1024,
            job_history: job::DEFAULT_HISTORY,
            accepting_jobs: true,
            geo_location: None,
            hostname: None,
        }
    }
}

impl PrinterConfig {
    /// Checks the Printer would describe itself consistently.
    ///
    /// [`Printer::start`] calls this, so a served Printer has always passed
    /// it; call it yourself to reject a bad configuration where it is read
    /// rather than where it is used.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("name", &self.name),
            ("manufacturer", &self.manufacturer),
            ("make_and_model", &self.make_and_model),
        ] {
            if value.trim().is_empty() {
                return Err(ConfigError::Blank { field });
            }
        }

        if !self.path.starts_with('/') {
            return Err(ConfigError::BadPath(self.path.clone()));
        }
        if self.max_copies < 1 {
            return Err(ConfigError::BadMaxCopies(self.max_copies));
        }

        if self.media.is_empty() {
            return Err(ConfigError::NothingOffered {
                attribute: "media-supported",
            });
        }
        if !self.media.contains(&self.default_media) {
            return Err(ConfigError::DefaultNotOffered {
                attribute: "media-default",
                value: self.default_media.name.to_string(),
            });
        }

        if self.document_formats.is_empty() {
            return Err(ConfigError::NothingOffered {
                attribute: "document-format-supported",
            });
        }
        if !self
            .document_formats
            .contains(&self.default_document_format)
        {
            return Err(ConfigError::DefaultNotOffered {
                attribute: "document-format-default",
                value: self.default_document_format.clone(),
            });
        }

        if self.resolutions.is_empty() {
            return Err(ConfigError::NothingOffered {
                attribute: "printer-resolution-supported",
            });
        }
        if !self.resolutions.contains(&self.default_resolution) {
            return Err(ConfigError::DefaultNotOffered {
                attribute: "printer-resolution-default",
                value: self.default_resolution.to_string(),
            });
        }

        // Not `raster_types`: what reaches the wire is filtered by colour, so
        // a monochrome Printer listing only colour rasters advertises none.
        if self.advertised_raster_types().is_empty() {
            return Err(ConfigError::NothingOffered {
                attribute: "pwg-raster-document-type-supported",
            });
        }

        Ok(())
    }

    /// Whether PDF is accepted, which turns on several conditionally required
    /// attributes.
    pub fn supports_pdf(&self) -> bool {
        self.document_formats.iter().any(|f| f == "application/pdf")
    }

    /// `print-color-mode-supported`.
    pub fn color_modes(&self) -> Vec<&'static str> {
        if self.color {
            vec!["auto", "color", "monochrome"]
        } else {
            vec!["auto", "monochrome"]
        }
    }

    pub fn default_color_mode(&self) -> &'static str {
        "auto"
    }

    /// `pwg-raster-document-type-supported`
    ///
    /// If a Printer is monochrome, we should not advertise color rasters.
    pub fn advertised_raster_types(&self) -> Vec<&'static str> {
        self.raster_types
            .iter()
            .copied()
            .filter(|kind| self.color || is_monochrome_raster(kind))
            .collect()
    }

    pub fn state_message(&self) -> &'static str {
        "Ready"
    }

    /// An IEEE 1284 device id for `printer-device-id`.
    pub fn device_id(&self) -> String {
        fn clean(value: &str) -> String {
            let cleaned: String = value
                .chars()
                .filter(|c| !matches!(c, ';' | ':'))
                .collect::<String>()
                .trim()
                .to_string();
            if cleaned.is_empty() {
                "Unknown".into()
            } else {
                cleaned
            }
        }

        let commands: Vec<&str> = self
            .document_formats
            .iter()
            .filter_map(|f| match f.as_str() {
                "image/pwg-raster" => Some("PWGRaster"),
                "application/pdf" => Some("PDF"),
                "image/jpeg" => Some("JPEG"),
                _ => None,
            })
            .collect();

        format!(
            "MFG:{};MDL:{};CMD:{};",
            clean(&self.manufacturer),
            clean(&self.make_and_model),
            commands.join(",")
        )
    }
}

/// Whether a PWG Raster type keyword describes a black-and-white raster.
fn is_monochrome_raster(kind: &str) -> bool {
    kind.starts_with("black_") || kind.starts_with("sgray_")
}

/// A job request.
#[derive(Debug, Clone)]
pub struct JobRequest {
    pub id: i32,
    pub uuid: Uuid,
    pub name: String,
    pub user: String,
    /// The address the submission came from.
    pub origin: IpAddr,
    /// The document's MIME media type, as resolved by the Printer.
    pub format: String,
    /// The document itself, already decompressed.
    pub document: Vec<u8>,
    /// Job Template attributes the Client sent.
    pub template: Vec<IppAttribute>,
}

impl JobRequest {
    /// Looks up a Job Template attribute the Client sent.
    pub fn attribute(&self, name: &str) -> Option<&IppValue> {
        self.template
            .iter()
            .find(|a| a.name().as_str() == name)
            .map(IppAttribute::value)
    }

    /// `copies`, defaulting to 1.
    pub fn copies(&self) -> i32 {
        match self.attribute(IppAttribute::COPIES) {
            Some(IppValue::Integer(n)) if *n > 0 => *n,
            _ => 1,
        }
    }

    /// The requested PWG media name, if any.
    pub fn media(&self) -> Option<&str> {
        match self.attribute(IppAttribute::MEDIA) {
            Some(IppValue::Keyword(name)) | Some(IppValue::NameWithoutLanguage(name)) => {
                Some(name.as_str())
            }
            _ => None,
        }
    }

    /// `sides`, defaulting to `one-sided`.
    pub fn sides(&self) -> &str {
        match self.attribute(IppAttribute::SIDES) {
            Some(IppValue::Keyword(value)) => value.as_str(),
            _ => "one-sided",
        }
    }
}

/// Why a submission was refused.
#[derive(Debug, Clone)]
pub struct Rejection {
    pub status: StatusCode,
    pub message: String,
}

impl Rejection {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Rejection {
            status,
            message: message.into(),
        }
    }

    /// The document is not something the Printer can read.
    pub fn bad_document(message: impl Into<String>) -> Self {
        Rejection::new(StatusCode::ClientErrorDocumentFormatError, message)
    }

    /// The request asks for something outside the Printer's limits.
    pub fn not_possible(message: impl Into<String>) -> Self {
        Rejection::new(
            StatusCode::ClientErrorAttributesOrValuesNotSupported,
            message,
        )
    }
}

/// The result of a successful job validation.
#[derive(Debug, Clone)]
pub struct Accepted<P> {
    /// Pages the job will produce, reported as `job-impressions`.
    pub impressions: i32,
    /// The format the Printer interpreted the document as.
    pub format: Option<String>,
    /// Arbitrary backend-specific metadata.
    pub plan: P,
}

/// What the Printer does with a job.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Carried from [`Backend::validate`] to [`Backend::print`].
    type Plan: Send + Sync + 'static;

    /// Inspects a submission while the Client waits.
    ///
    /// This runs inside the Client's request, so it should be cheap. Actual
    /// printing must be done in [`Backend::print`]. Returning an error refuses
    /// the job and tells the Client a reason.
    async fn validate(&self, request: &JobRequest) -> Result<Accepted<Self::Plan>, Rejection>;

    /// Prints an accepted job on the worker.
    ///
    /// An error here aborts the job. It is recorded but there is no guarantee
    /// the Client will receive it.
    async fn print(&self, request: JobRequest, plan: Self::Plan) -> Result<(), String>;

    /// Handles `Identify-Printer`.
    ///
    /// On physical devices it flashes a light or makes a sound.
    async fn identify(&self, message: Option<&str>) {
        match message {
            Some(message) => tracing::info!(message, "identify-printer"),
            None => tracing::info!("identify-printer"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_config;

    #[test]
    fn the_supported_operations_are_the_eleven_ipp_everywhere_requires() {
        assert_eq!(Operations::SUPPORTED.len(), 11);
        for required in [
            0x0002u16, 0x0004, 0x0005, 0x0006, 0x0008, 0x0009, 0x000A, 0x000B, 0x0039, 0x003B,
            0x003C,
        ] {
            assert!(
                Operations::SUPPORTED.contains(&required),
                "missing operation {required:#06x}"
            );
        }
    }

    #[test]
    fn the_device_id_is_built_from_what_the_printer_accepts() {
        let config = PrinterConfig {
            manufacturer: "TeamTeam".into(),
            make_and_model: "Watermark Printer".into(),
            ..PrinterConfig::default()
        };
        assert_eq!(
            config.device_id(),
            "MFG:TeamTeam;MDL:Watermark Printer;CMD:PDF,JPEG,PWGRaster;"
        );
    }

    #[test]
    fn device_id_values_cannot_break_the_format() {
        let config = PrinterConfig {
            manufacturer: "Awkward;Name:Ltd".into(),
            make_and_model: "  ".into(),
            ..PrinterConfig::default()
        };
        let id = config.device_id();
        assert!(id.contains("MFG:AwkwardNameLtd;"), "{id}");
        // An empty field would produce `MDL:;`, which still parses, but a
        // placeholder is more use to whoever reads it.
        assert!(id.contains("MDL:Unknown;"), "{id}");
    }

    #[test]
    fn colour_support_decides_the_colour_modes() {
        // Monochrome by default: a Client told the printer is colour sends
        // colour raster, which is several times larger for nothing.
        let mut config = PrinterConfig::default();
        assert!(!config.color);
        assert!(!config.color_modes().contains(&"color"));
        assert!(config.color_modes().contains(&"monochrome"));

        config.color = true;
        assert!(config.color_modes().contains(&"color"));
    }

    #[test]
    fn a_monochrome_printer_does_not_offer_colour_raster() {
        let mut config = PrinterConfig {
            color: false,
            ..PrinterConfig::default()
        };
        let kinds = config.advertised_raster_types();
        assert!(kinds.contains(&"sgray_8"), "{kinds:?}");
        assert!(kinds.contains(&"black_1"), "{kinds:?}");
        for colour in ["srgb_8", "cmyk_8", "adobe-rgb_8", "srgb_16"] {
            assert!(
                !kinds.contains(&colour),
                "{colour} offered by a mono printer"
            );
        }

        // With colour claimed, the whole list is offered again.
        config.color = true;
        let kinds = config.advertised_raster_types();
        assert!(kinds.contains(&"srgb_8"), "{kinds:?}");
        assert_eq!(kinds.len(), config.raster_types.len());
    }

    #[test]
    fn the_monochrome_raster_families_are_recognised() {
        for kind in [
            "black_1", "black_8", "black_16", "sgray_1", "sgray_8", "sgray_16",
        ] {
            assert!(is_monochrome_raster(kind), "{kind}");
        }
        for kind in ["srgb_8", "cmyk_8", "adobe-rgb_16", "rgb_8", "device3_8"] {
            assert!(!is_monochrome_raster(kind), "{kind}");
        }
    }

    #[test]
    fn the_default_configuration_is_consistent_once_it_has_been_named() {
        // `default()` deliberately leaves the identity blank, so it is a
        // starting point rather than a servable Printer.
        assert_eq!(
            PrinterConfig::default().validate(),
            Err(ConfigError::Blank { field: "name" })
        );

        // Naming it is the only thing it needs: everything else the default
        // chooses is consistent.
        assert_eq!(test_config().validate(), Ok(()));
    }

    #[test]
    fn each_identifying_field_must_be_more_than_whitespace() {
        for field in ["name", "manufacturer", "make_and_model"] {
            let mut config = test_config();
            match field {
                "name" => config.name = "   ".into(),
                "manufacturer" => config.manufacturer = String::new(),
                _ => config.make_and_model = "\t".into(),
            }
            assert_eq!(
                config.validate(),
                Err(ConfigError::Blank { field }),
                "{field}"
            );
        }
    }

    #[test]
    fn the_descriptive_fields_may_be_left_blank() {
        // Not stated is a legitimate answer for where a Printer is and who
        // runs it; IPP sends an empty text value.
        let config = PrinterConfig {
            info: String::new(),
            location: String::new(),
            organization: String::new(),
            organizational_unit: String::new(),
            ..test_config()
        };
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn a_default_outside_what_is_offered_is_refused() {
        let cases: Vec<(&str, PrinterConfig)> = vec![
            (
                "media-default",
                PrinterConfig {
                    default_media: *media::find("iso_a3_297x420mm").unwrap(),
                    media: vec![*media::find("iso_a4_210x297mm").unwrap()],
                    ..test_config()
                },
            ),
            (
                "document-format-default",
                PrinterConfig {
                    default_document_format: "application/postscript".into(),
                    ..test_config()
                },
            ),
            (
                "printer-resolution-default",
                PrinterConfig {
                    default_resolution: 1200,
                    ..test_config()
                },
            ),
        ];
        for (attribute, config) in cases {
            assert_eq!(
                config.validate(),
                Err(ConfigError::DefaultNotOffered {
                    attribute,
                    value: match attribute {
                        "media-default" => "iso_a3_297x420mm".to_string(),
                        "document-format-default" => "application/postscript".to_string(),
                        _ => "1200".to_string(),
                    },
                }),
                "{attribute}"
            );
        }
    }

    #[test]
    fn offering_nothing_at_all_is_refused() {
        let empty = [
            (
                "media-supported",
                PrinterConfig {
                    media: Vec::new(),
                    ..test_config()
                },
            ),
            (
                "document-format-supported",
                PrinterConfig {
                    document_formats: Vec::new(),
                    ..test_config()
                },
            ),
            (
                "printer-resolution-supported",
                PrinterConfig {
                    resolutions: Vec::new(),
                    ..test_config()
                },
            ),
            (
                "pwg-raster-document-type-supported",
                PrinterConfig {
                    raster_types: Vec::new(),
                    ..test_config()
                },
            ),
        ];
        for (attribute, config) in empty {
            assert_eq!(
                config.validate(),
                Err(ConfigError::NothingOffered { attribute }),
                "{attribute}"
            );
        }
    }

    #[test]
    fn a_monochrome_printer_that_can_only_decode_colour_raster_is_refused() {
        // The colour types are withheld from a monochrome Printer, so this
        // one would advertise an empty set while `raster_types` looks full.
        let config = PrinterConfig {
            color: false,
            raster_types: vec!["srgb_8", "cmyk_8"],
            ..test_config()
        };
        assert!(!config.raster_types.is_empty());
        assert_eq!(
            config.validate(),
            Err(ConfigError::NothingOffered {
                attribute: "pwg-raster-document-type-supported",
            })
        );
    }

    #[test]
    fn a_path_that_cannot_build_a_uri_and_a_copy_count_below_one_are_refused() {
        let config = PrinterConfig {
            path: "ipp/print".into(),
            ..test_config()
        };
        assert_eq!(
            config.validate(),
            Err(ConfigError::BadPath("ipp/print".into()))
        );

        let config = PrinterConfig {
            max_copies: 0,
            ..test_config()
        };
        assert_eq!(config.validate(), Err(ConfigError::BadMaxCopies(0)));
    }

    #[test]
    fn job_template_accessors_fall_back_to_the_defaults() {
        let request = JobRequest {
            id: 1,
            uuid: Uuid::nil(),
            name: "job".into(),
            user: "tester".into(),
            origin: "127.0.0.1".parse().unwrap(),
            format: "application/pdf".into(),
            document: Vec::new(),
            template: vec![
                attributes::attr("copies", IppValue::Integer(3)),
                attributes::attr("media", attributes::keyword("iso_a4_210x297mm")),
            ],
        };
        assert_eq!(request.copies(), 3);
        assert_eq!(request.media(), Some("iso_a4_210x297mm"));
        assert_eq!(request.sides(), "one-sided");

        let empty = JobRequest {
            template: Vec::new(),
            ..request
        };
        assert_eq!(empty.copies(), 1);
        assert_eq!(empty.media(), None);
    }

    #[test]
    fn a_nonsensical_copy_count_falls_back_rather_than_being_trusted() {
        let request = JobRequest {
            id: 1,
            uuid: Uuid::nil(),
            name: "job".into(),
            user: "tester".into(),
            origin: "127.0.0.1".parse().unwrap(),
            format: "application/pdf".into(),
            document: Vec::new(),
            template: vec![attributes::attr("copies", IppValue::Integer(-5))],
        };
        assert_eq!(request.copies(), 1);
    }
}
