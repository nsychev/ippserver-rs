//! DNS-SD advertising
//!
//! For machines that already run Avahi or Bonjour, register the Printer
//! with that system responder. In other cases, use [`Advertisement`]
//! to run a built-in mDNS stack that advertises the Printer via [`mdns-sd`].
//!
//! [`mdns-sd`]: https://crates.io/crates/mdns-sd

use std::net::IpAddr;

use crate::attributes::PrinterContext;

pub const SERVICE_TYPE: &str = "_ipp._tcp.local.";

/// The TXT keys of IPP Everywhere Table 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecords {
    pairs: Vec<(String, String)>,
}

impl TxtRecords {
    pub fn new(ctx: &PrinterContext<'_>) -> Self {
        let config = ctx.config;

        // The resource path without its leading slash.
        let queue = config.path.trim_start_matches('/').to_string();

        let mut pairs = vec![
            ("txtvers".to_string(), "1".to_string()),
            ("rp".to_string(), queue),
            ("ty".to_string(), config.make_and_model.clone()),
            ("adminurl".to_string(), ctx.http_uri("/")),
            ("note".to_string(), config.location.clone()),
            ("priority".to_string(), "50".to_string()),
            ("UUID".to_string(), config.uuid.to_string()),
            // No TLS: this Printer speaks plain IPP only.
            ("TLS".to_string(), "none".to_string()),
            ("air".to_string(), "none".to_string()),
            ("Color".to_string(), boolean(config.color)),
            // One document per job, printed as it arrives.
            ("Duplex".to_string(), boolean(false)),
            ("Copies".to_string(), boolean(true)),
            ("Collate".to_string(), boolean(false)),
            ("Bind".to_string(), boolean(false)),
            ("Punch".to_string(), boolean(false)),
            ("Sort".to_string(), boolean(false)),
            ("Staple".to_string(), boolean(false)),
            ("PaperCustom".to_string(), boolean(false)),
            ("PaperMax".to_string(), paper_max(largest_sheet(config))),
            ("pdl".to_string(), pdl(&config.document_formats)),
        ];
        pairs.retain(|(_, value)| !value.is_empty());
        TxtRecords { pairs }
    }

    /// The key/value pairs, in order.
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// The records as an Avahi static service file.
    ///
    /// Write this to `/etc/avahi/services/<name>.service` to make Avahi
    /// advertise the Printer. Avahi reloads the directory by itself.
    pub fn avahi_service_file(&self, instance: &str, port: u16) -> String {
        let mut out = String::from(
            "<?xml version=\"1.0\" standalone='no'?>\n\
             <!DOCTYPE service-group SYSTEM \"avahi-service.dtd\">\n\
             <service-group>\n",
        );
        out.push_str(&format!("  <name>{}</name>\n", escape(instance)));
        out.push_str("  <service>\n    <type>_ipp._tcp</type>\n");
        out.push_str(&format!("    <port>{port}</port>\n"));
        for (key, value) in &self.pairs {
            out.push_str(&format!(
                "    <txt-record>{}={}</txt-record>\n",
                escape(key),
                escape(value)
            ));
        }
        out.push_str("  </service>\n</service-group>\n");
        out
    }
}

fn boolean(value: bool) -> String {
    if value { "T" } else { "F" }.to_string()
}

/// The largest sheet offered, which is what `PaperMax` describes.
fn largest_sheet(config: &crate::PrinterConfig) -> &crate::media::MediaSize {
    config
        .media
        .iter()
        .max_by_key(|m| m.width.max(m.height))
        .unwrap_or(&config.default_media)
}

// `PaperMax` bucket is determined by the longer edge of a sheet.
fn paper_max(media: &crate::media::MediaSize) -> String {
    // Dimensions are hundredths of a millimetre.
    let longest = media.width.max(media.height);
    if longest < 29_700 {
        "<legal-A4"
    } else if longest <= 35_560 {
        "legal-A4"
    } else if longest <= 59_400 {
        "isoC-A2"
    } else {
        ">isoC-A2"
    }
    .to_string()
}

fn pdl(formats: &[String]) -> String {
    let mut out = String::new();
    for format in formats {
        if format == "application/octet-stream" {
            // Section 4.2.4.3 forbids listing it.
            continue;
        }
        let candidate = if out.is_empty() {
            format.clone()
        } else {
            format!("{out},{format}")
        };
        // "pdl=" plus the value must fit in 255 octets.
        if candidate.len() + 4 > 255 {
            break;
        }
        out = candidate;
    }
    out
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(feature = "mdns-sd")]
pub use builtin::Advertisement;

#[cfg(feature = "mdns-sd")]
mod builtin {
    use super::{SERVICE_TYPE, TxtRecords};
    use std::collections::HashMap;
    use std::net::IpAddr;

    /// A registration held for as long as the value lives.
    ///
    /// Dropping it withdraws the service, so a Printer that stops answering
    /// stops being advertised.
    pub struct Advertisement {
        daemon: mdns_sd::ServiceDaemon,
        fullname: String,
    }

    impl Advertisement {
        /// Advertises a Printer on the local network.
        ///
        /// `instance` is the name people will see. `hostname` should be this
        /// machine's name without a domain; `.local.` is appended.
        pub fn register(
            instance: &str,
            hostname: &str,
            addresses: &[IpAddr],
            port: u16,
            records: &TxtRecords,
        ) -> Result<Self, String> {
            let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| e.to_string())?;

            let properties: HashMap<String, String> = records
                .pairs()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let host = format!("{}.local.", hostname.trim_end_matches(".local."));
            let info = mdns_sd::ServiceInfo::new(
                SERVICE_TYPE,
                instance,
                &host,
                addresses,
                port,
                properties,
            )
            .map_err(|e| e.to_string())?;

            let fullname = info.get_fullname().to_string();
            daemon.register(info).map_err(|e| e.to_string())?;
            tracing::info!(service = %fullname, "advertised over DNS-SD");

            Ok(Advertisement { daemon, fullname })
        }
    }

    impl Drop for Advertisement {
        fn drop(&mut self) {
            let _ = self.daemon.unregister(&self.fullname);
            let _ = self.daemon.shutdown();
        }
    }
}

/// Addresses worth advertising: everything routable on this host except loopback.
pub fn advertisable(addresses: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    addresses
        .into_iter()
        .filter(|address| !address.is_loopback() && !address.is_unspecified())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{context, test_config};

    fn records() -> TxtRecords {
        let config = test_config();
        TxtRecords::new(&context(&config))
    }

    fn value(records: &TxtRecords, key: &str) -> Option<String> {
        records
            .pairs()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn the_required_keys_of_table_3_are_present() {
        let records = records();
        for key in ["txtvers", "rp", "ty", "adminurl", "UUID", "TLS", "pdl"] {
            assert!(value(&records, key).is_some(), "missing {key}");
        }
        assert_eq!(value(&records, "txtvers").as_deref(), Some("1"));
        assert_eq!(value(&records, "TLS").as_deref(), Some("none"));
    }

    #[test]
    fn the_queue_name_has_no_leading_slash() {
        // Table 3: "the resource path portion of the Printer URI without the
        // leading slash".
        assert_eq!(value(&records(), "rp").as_deref(), Some("ipp/print"));
    }

    #[test]
    fn the_uuid_carries_no_urn_prefix() {
        let uuid = value(&records(), "UUID").unwrap();
        assert!(!uuid.starts_with("urn:"), "{uuid}");
        assert_eq!(uuid.len(), 36, "{uuid}");
    }

    #[test]
    fn colour_is_reported_as_the_printer_reports_it() {
        let mut config = test_config();
        config.color = false;
        assert_eq!(
            value(&TxtRecords::new(&context(&config)), "Color").as_deref(),
            Some("F")
        );
        config.color = true;
        assert_eq!(
            value(&TxtRecords::new(&context(&config)), "Color").as_deref(),
            Some("T")
        );
    }

    #[test]
    fn empty_values_are_left_out_rather_than_advertised_blank() {
        let config = test_config();
        // The fixture has a location; without one the key disappears.
        assert!(value(&TxtRecords::new(&context(&config)), "note").is_some());

        let mut bare = test_config();
        bare.location = String::new();
        assert!(value(&TxtRecords::new(&context(&bare)), "note").is_none());
    }

    #[test]
    fn the_pdl_list_omits_octet_stream_and_fits_the_txt_limit() {
        assert_eq!(
            pdl(&[
                "image/pwg-raster".into(),
                "application/octet-stream".into(),
                "image/jpeg".into(),
            ]),
            "image/pwg-raster,image/jpeg"
        );

        // A list longer than a TXT pair allows is truncated at a whole type.
        let many: Vec<String> = (0..40)
            .map(|n| format!("application/x-type-{n:02}"))
            .collect();
        let listed = pdl(&many);
        assert!(listed.len() + 4 <= 255, "{} octets", listed.len());
        assert!(!listed.ends_with(','));
        // And what survives is still parseable as whole media types.
        for entry in listed.split(',') {
            assert!(entry.starts_with("application/x-type-"), "{entry}");
        }
    }

    #[test]
    fn paper_max_buckets_follow_the_longer_edge() {
        let size = |name: &str| *crate::media::find(name).unwrap();
        assert_eq!(paper_max(&size("iso_a5_148x210mm")), "<legal-A4");
        assert_eq!(paper_max(&size("iso_a4_210x297mm")), "legal-A4");
        assert_eq!(paper_max(&size("na_legal_8.5x14in")), "legal-A4");
        assert_eq!(paper_max(&size("iso_a3_297x420mm")), "isoC-A2");
    }

    #[test]
    fn the_avahi_service_file_is_well_formed_and_escaped() {
        let mut config = test_config();
        config.location = "Hall <2> & 3".into();
        let records = TxtRecords::new(&context(&config));
        let file = records.avahi_service_file("Test & Printer", 6631);

        assert!(file.starts_with("<?xml"));
        assert!(file.contains("<type>_ipp._tcp</type>"));
        assert!(file.contains("<port>6631</port>"));
        assert!(file.contains("<name>Test &amp; Printer</name>"));
        assert!(file.contains("note=Hall &lt;2&gt; &amp; 3"));
        assert!(file.trim_end().ends_with("</service-group>"));
        // One record per pair.
        assert_eq!(file.matches("<txt-record>").count(), records.pairs().len());
    }

    #[test]
    fn loopback_is_not_advertised() {
        let addresses = vec![
            "127.0.0.1".parse().unwrap(),
            "192.168.1.5".parse().unwrap(),
            "::1".parse().unwrap(),
            "0.0.0.0".parse().unwrap(),
        ];
        assert_eq!(
            advertisable(addresses),
            vec!["192.168.1.5".parse::<IpAddr>().unwrap()]
        );
    }
}
