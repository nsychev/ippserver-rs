//! A printer that writes every job it accepts into a folder.
//!
//! ```bash
//! cargo run --example save-to-folder -- ./jobs 0.0.0.0:6310
//! lp -h localhost:6310 -d ipp://localhost:6310/ipp/print document.pdf
//! ```
//!
//! Both arguments are optional; the defaults are `./jobs` and
//! `0.0.0.0:6310`. We don't use port 631 as it requires root privileges.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ippserver::{Accepted, Backend, JobRequest, Printer, PrinterConfig, Rejection, serve};
use uuid::Uuid;

struct Folder {
    directory: PathBuf,
}

#[async_trait::async_trait]
impl Backend for Folder {
    /// validate will return the path to the file this job will be written to.
    type Plan = PathBuf;

    async fn validate(&self, job: &JobRequest) -> Result<Accepted<PathBuf>, Rejection> {
        if job.document.is_empty() {
            return Err(Rejection::bad_document("the document is empty"));
        }

        let format = sniff(&job.document);
        let extension = extension(format.unwrap_or(&job.format));

        let file = self
            .directory
            .join(format!("{:05}-{}{}", job.id, stem(job), extension));

        Ok(Accepted {
            // https://datatracker.ietf.org/doc/html/rfc8011#section-2.3.4
            // We don't print anything, so returning 1 is fine.
            impressions: 1,

            // The format the document was interpreted as.
            format: format.map(str::to_owned),
            plan: file,
        })
    }

    async fn print(&self, job: JobRequest, file: PathBuf) -> Result<(), String> {
        tokio::fs::write(&file, &job.document)
            .await
            .map_err(|error| format!("cannot write {}: {error}", file.display()))?;

        println!(
            "job {} from {} ({}): {} bytes -> {}",
            job.id,
            job.user,
            job.origin,
            job.document.len(),
            file.display()
        );
        Ok(())
    }
}

/// Sanitize the job name to use as a file name.
fn stem(job: &JobRequest) -> String {
    let safe: String = job
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();

    // `..`, `.` and a leading dot all stop being special once there is a
    // `{id}-` prefix in front, but an empty name still needs a stand-in.
    if safe.trim_matches(['_', '.']).is_empty() {
        "job".into()
    } else {
        safe
    }
}

/// The format a document really is, from the bytes it starts with.
fn sniff(document: &[u8]) -> Option<&'static str> {
    if document.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if document.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if document.starts_with(b"RaS2") || document.starts_with(b"2SaR") {
        // The PWG Raster sync word, in either byte order (PWG 5102.4
        // section 4.1).
        Some("image/pwg-raster")
    } else {
        None
    }
}

/// A file extension for a document format the Printer accepts.
fn extension(format: &str) -> &'static str {
    match format {
        "application/pdf" => ".pdf",
        "image/jpeg" => ".jpg",
        "image/pwg-raster" => ".pwg",
        _ => ".bin",
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let directory = PathBuf::from(arguments.next().unwrap_or_else(|| "./jobs".into()));
    let address: SocketAddr = arguments
        .next()
        .unwrap_or_else(|| "0.0.0.0:6310".into())
        .parse()
        .expect("the second argument should be an address, e.g. 0.0.0.0:6310");

    std::fs::create_dir_all(&directory)?;
    let directory = std::fs::canonicalize(&directory)?;

    let config = PrinterConfig {
        name: "Folder".into(),
        info: format!("Saves every job in {}", directory.display()),
        manufacturer: "ippserver".into(),
        make_and_model: "Folder Printer".into(),
        uuid: printer_uuid(&directory),
        // This service can handle any document, but let's tell the clients only required ones.
        document_formats: vec![
            "application/pdf".into(),
            "image/jpeg".into(),
            "image/pwg-raster".into(),
        ],
        ..PrinterConfig::default()
    };

    let path = config.path.clone();
    let printer = match Printer::start(
        config,
        Folder {
            directory: directory.clone(),
        },
    ) {
        Ok(printer) => printer,
        Err(error) => {
            // `?` would print the Debug form; the message is the useful part.
            eprintln!("cannot serve this configuration: {error}");
            std::process::exit(1);
        }
    };

    println!("saving jobs in {}", directory.display());
    println!("printing to ipp://{address}{path}");

    serve(address, printer).await
}

/// A `printer-uuid` that stays the same between restarts.
fn printer_uuid(directory: &Path) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!(
            "urn:ippserver:example:save-to-folder:{}",
            directory.display()
        )
        .as_bytes(),
    )
}
