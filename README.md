# ippserver

An [IPP/2.0][ippbase] and [IPP Everywhere™][ippeve] conformant printer
service in Rust. It supports driverless printing: all modern operating
systems can connect to the service out of the box.

[ippbase]: https://ftp.pwg.org/pub/pwg/standards/std-ippbase23-20241108-5100.12.pdf
[ippeve]: https://ftp.pwg.org/pub/pwg/candidates/cs-ippeve11-20200515-5100.14.pdf

## Usage

Implement `Backend`. It has two methods `validate` and `print`:

```rust
use ippserver::{Accepted, Backend, JobRequest, Rejection};

struct MyPrinter;

#[async_trait::async_trait]
impl Backend for MyPrinter {
    type Plan = ();

    /// Synchronous part: the client waits for a response, you may refuse the
    /// job immediately.
    async fn validate(&self, job: &JobRequest) -> Result<Accepted<()>, Rejection> {
        Ok(Accepted { impressions: 1, format: None, plan: () })
    }

    /// Asynchronous part, runs on worker when the job is already queued.
    async fn print(&self, job: JobRequest, _plan: ()) -> Result<(), String> {
        println!("printing {} bytes", job.document.len());
        Ok(())
    }
}
```

Then serve it:

```rust,no_run
# use ippserver::*;
# async fn example<B: Backend>(backend: B) -> Result<(), Box<dyn std::error::Error>> {
// `default()` leaves the identifying fields blank, because there is no
// sensible default for someone else's printer.
let config = PrinterConfig {
    name: "Upstairs".into(),
    manufacturer: "Acme".into(),
    make_and_model: "Acme Jet 100".into(),
    ..PrinterConfig::default()
};
let printer = Printer::start(config, backend)?;
serve("0.0.0.0:631".parse().unwrap(), printer).await?;
# Ok(())
# }
```

Check out an example: [`save-to-folder.rs`](examples/save-to-folder.rs)
just writes every job into a folder.

```bash
cargo run --example save-to-folder -- ./jobs 0.0.0.0:6310
lp -h localhost:6310 -d ipp://localhost:6310/ipp/print document.pdf
```

## Conformance

Checked against the CUPS IPP Everywhere test suite:

```bash
ipptool -V 2.0 -tf document-a4.pdf ipp://localhost:631/ipp/print ipp-everywhere.test
```

## Discovery

IPP Everywhere requires printers to support DNS-SD for automatic discovery.
This feature is _off_ by default.

If you need to enable it, use `discovery::TxtRecord` to build the DNS records.
Then you need to advertise it:

* If your host already runs Avahi or Bonjour, register the service using it.
  `TxtRecords::avahi_service_file` can be used for a record in `/etc/avahi/services`.

* Otherwise you can use `mdns-sd` feature that brings in [`mdns-sd`] crate and
  advertise the printing service using `Advertisement`.

[`mdns-sd`]: https://crates.io/crates/mdns-sd

## Acknowledgements

This project was inspired by [`ippper.rs`](https://github.com/ArcticLampyrid/ippper.rs).
