//! Media size definition.
//!
//! PWG 5101.1 defines a self-describing name for each sheet size. Sizes
//! are defined in hundredths of a millimetre.

/// A sheet this printer accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaSize {
    /// The PWG self-describing name.
    pub name: &'static str,
    /// Width in hundredths of a millimetre.
    pub width: i32,
    /// Height in hundredths of a millimetre.
    pub height: i32,
}

impl MediaSize {
    /// The size in PDF points.
    pub fn points(&self) -> (f64, f64) {
        (hmm_to_points(self.width), hmm_to_points(self.height))
    }
}

/// Converts hundredths of a millimetre to PDF points.
pub fn hmm_to_points(hmm: i32) -> f64 {
    // 1 inch is 2540 hundredths of a millimetre and 72 points.
    hmm as f64 * 72.0 / 2540.0
}

/// Supported sheet names.
pub const SUPPORTED: &[MediaSize] = &[
    MediaSize {
        name: "iso_a3_297x420mm",
        width: 29700,
        height: 42000,
    },
    MediaSize {
        name: "iso_a4_210x297mm",
        width: 21000,
        height: 29700,
    },
    MediaSize {
        name: "iso_a5_148x210mm",
        width: 14800,
        height: 21000,
    },
    MediaSize {
        name: "iso_a6_105x148mm",
        width: 10500,
        height: 14800,
    },
    MediaSize {
        name: "iso_b5_176x250mm",
        width: 17600,
        height: 25000,
    },
    MediaSize {
        name: "iso_c5_162x229mm",
        width: 16200,
        height: 22900,
    },
    MediaSize {
        name: "iso_dl_110x220mm",
        width: 11000,
        height: 22000,
    },
    MediaSize {
        name: "jis_b5_182x257mm",
        width: 18200,
        height: 25700,
    },
    MediaSize {
        name: "na_letter_8.5x11in",
        width: 21590,
        height: 27940,
    },
    MediaSize {
        name: "na_legal_8.5x14in",
        width: 21590,
        height: 35560,
    },
    MediaSize {
        name: "na_executive_7.25x10.5in",
        width: 18415,
        height: 26670,
    },
    MediaSize {
        name: "na_ledger_11x17in",
        width: 27940,
        height: 43180,
    },
    MediaSize {
        name: "na_index-4x6_4x6in",
        width: 10160,
        height: 15240,
    },
    MediaSize {
        name: "na_number-10_4.125x9.5in",
        width: 10477,
        height: 24130,
    },
    MediaSize {
        name: "na_monarch_3.875x7.5in",
        width: 9842,
        height: 19050,
    },
];

/// The sheet used when none is configured.
pub const DEFAULT: &str = "iso_a4_210x297mm";

/// The unprintable edge claimed on all four sides, in hundredths of a
/// millimetre.
pub const MARGIN: i32 = 423;

/// `media-type-supported`
pub const TYPES: &[&str] = &["stationery"];

/// `media-source-supported`
pub const SOURCES: &[&str] = &["auto", "main"];

pub fn find(name: &str) -> Option<&'static MediaSize> {
    SUPPORTED.iter().find(|m| m.name == name)
}

pub fn default_size() -> &'static MediaSize {
    find(DEFAULT).expect("the default media must be a supported one")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a4_and_letter_convert_to_the_expected_points() {
        let (w, h) = find("iso_a4_210x297mm").unwrap().points();
        assert!((w - 595.28).abs() < 0.01, "{w}");
        assert!((h - 841.89).abs() < 0.01, "{h}");

        let (w, h) = find("na_letter_8.5x11in").unwrap().points();
        assert!((w - 612.0).abs() < 0.01, "{w}");
        assert!((h - 792.0).abs() < 0.01, "{h}");
    }

    #[test]
    fn every_name_is_self_describing_and_matches_its_dimensions() {
        for media in SUPPORTED {
            // name is class_name_WxH{in,mm}
            let parts: Vec<&str> = media.name.split('_').collect();
            assert!(parts.len() >= 3, "{}", media.name);
            let size = parts.last().unwrap();

            let (dims, factor) = if let Some(d) = size.strip_suffix("mm") {
                (d, 100.0) // mm -> hundredths of a mm
            } else if let Some(d) = size.strip_suffix("in") {
                (d, 2540.0)
            } else {
                panic!("{} has no unit", media.name);
            };

            let (w, h) = dims.split_once('x').expect(media.name);
            let w: f64 = w.parse().expect(media.name);
            let h: f64 = h.parse().expect(media.name);

            // Inch sizes are rounded down to a whole hundredth of a mm.
            assert!(
                (w * factor - media.width as f64).abs() <= 1.0,
                "{}: {} vs {}",
                media.name,
                w * factor,
                media.width
            );
            assert!(
                (h * factor - media.height as f64).abs() <= 1.0,
                "{}: {} vs {}",
                media.name,
                h * factor,
                media.height
            );
        }
    }

    #[test]
    fn the_default_is_a_supported_size() {
        assert!(find(DEFAULT).is_some());
        assert_eq!(default_size().name, DEFAULT);
    }

    #[test]
    fn only_one_media_type_is_offered() {
        assert_eq!(TYPES, &["stationery"]);
    }

    #[test]
    fn unknown_sheets_are_not_invented() {
        assert!(find("iso_a0_841x1189mm").is_none());
        assert!(find("").is_none());
    }
}
