//! The HTTP transport.
//!
//! IPP uses HTTP POST requests with certain requirements set by PWG 5100.12
//! section 6 and IPP Everywhere section 5.1.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode as HttpStatus, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use chrono::{DateTime, Timelike as _, Utc};

use crate::{Backend, IppSummary, Printer, RequestContext, icons};

const IPP_CONTENT_TYPE: &str = "application/ipp";
const ICON_MAX_AGE: u32 = 86_400;

/// Logger name for the access logs.
const ACCESS: &str = "ippserver::access";

/// Max log field size.
const MAX_LOGGED: usize = 200;

pub fn router<B: Backend>(printer: Arc<Printer<B>>) -> Router {
    Router::new().fallback(any(handle::<B>)).with_state(printer)
}

pub async fn serve<B: Backend>(
    address: SocketAddr,
    printer: Arc<Printer<B>>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(
        listener,
        router(printer).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

async fn handle<B: Backend>(State(printer): State<Arc<Printer<B>>>, request: Request) -> Response {
    // `into_make_service_with_connect_info` puts the peer address in an extension.
    let origin = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

    let (parts, body) = request.into_parts();
    let started = Instant::now();

    let mut summary = None;
    let response = respond(&printer, origin, &parts, body, &mut summary).await;

    tracing::debug!(
        target: ACCESS,
        "{}",
        access_line(
            origin,
            &parts,
            response.status(),
            summary.as_ref(),
            started.elapsed(),
        )
    );

    response
}

async fn respond<B: Backend>(
    printer: &Arc<Printer<B>>,
    origin: IpAddr,
    parts: &Parts,
    body: Body,
    summary: &mut Option<IppSummary>,
) -> Response {
    // Section 6.5: validate the Host header before acting on anything.
    let Some(host) = validated_host(&parts.headers, printer.config()) else {
        return text(HttpStatus::BAD_REQUEST, "unrecognised Host header");
    };

    match parts.method {
        Method::POST => {
            if !declares_a_body(&parts.headers) {
                return text(
                    HttpStatus::BAD_REQUEST,
                    "Content-Length or Transfer-Encoding is required",
                );
            }
            if !is_ipp(&parts.headers) {
                return text(
                    HttpStatus::BAD_REQUEST,
                    "Content-Type must be application/ipp",
                );
            }

            let limit = printer.config().max_document_bytes;
            let body = match axum::body::to_bytes(body, limit).await {
                Ok(body) => body,
                Err(_) => {
                    return text(
                        HttpStatus::PAYLOAD_TOO_LARGE,
                        "the request is larger than this printer accepts",
                    );
                }
            };

            let context = RequestContext { origin, host };
            let exchange = printer.handle(body, &context).await;
            *summary = Some(exchange.summary);

            (
                HttpStatus::OK,
                [
                    (header::CONTENT_TYPE, IPP_CONTENT_TYPE),
                    // Section 6.1: an IPP response is never cacheable.
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                Body::from(exchange.response),
            )
                .into_response()
        }
        Method::GET | Method::HEAD => {
            let path = parts.uri.path();
            if let Some(icon) = icons::find(path) {
                return icon_response(icon, &parts.headers, printer.started_at(), &parts.method);
            }
            if path == "/" {
                return status_page(printer, &parts.method);
            }
            text(HttpStatus::NOT_FOUND, "not found")
        }
        _ => (
            HttpStatus::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD, POST")],
            "method not allowed",
        )
            .into_response(),
    }
}

fn access_line(
    origin: IpAddr,
    parts: &Parts,
    status: HttpStatus,
    summary: Option<&IppSummary>,
    took: Duration,
) -> String {
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_string(), ToString::to_string);
    let mut line = format!(
        "{origin} \"{method} {target} {version:?}\" {status} {took:.1?} host={host}",
        method = parts.method,
        version = parts.version,
        status = status.as_u16(),
        host = header_value(&parts.headers, &header::HOST)
            .map_or_else(|| "-".to_string(), |value| quoted(&value)),
    );

    if let Some(agent) = header_value(&parts.headers, &header::USER_AGENT) {
        line.push_str(&format!(" ua={}", quoted(&agent)));
    }

    let Some(summary) = summary else {
        return line;
    };

    if let Some(operation) = summary.operation {
        match summary.operation_name() {
            Some(name) => line.push_str(&format!(" ipp={name}")),
            None => line.push_str(&format!(" ipp={operation:#06x}")),
        }
    }
    if let Some(name) = summary.status_name() {
        line.push_str(&format!(" ipp-status={name}"));
    }
    if let Some(id) = summary.request_id {
        line.push_str(&format!(" request-id={id}"));
    }
    if let Some(id) = summary.job_id {
        line.push_str(&format!(" job={id}"));
    }
    if let Some(user) = &summary.user {
        line.push_str(&format!(" user={}", quoted(user)));
    }
    if let Some(format) = &summary.document_format {
        line.push_str(&format!(" format={}", quoted(format)));
    }
    if let Some(name) = &summary.job_name {
        line.push_str(&format!(" job-name={}", quoted(name)));
    }
    line
}

fn header_value(headers: &HeaderMap, name: &header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Sanitizes client-supplied values.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars().take(MAX_LOGGED) {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.extend(c.escape_debug()),
            c => out.push(c),
        }
    }
    if value.chars().count() > MAX_LOGGED {
        out.push('\u{2026}');
    }
    out.push('"');
    out
}

/// Returns the `Host` value if this Printer answers to it.
///
/// PWG 5100.12 Section 6.5 requires to validate `Host` header.
/// We allow IP literals, `localhost`, mDNS `.local` names, and the configured hostname.
fn validated_host(headers: &HeaderMap, config: &crate::PrinterConfig) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?.trim();
    if host.is_empty() {
        return None;
    }

    // Split the authority, minding an IPv6 literal's brackets.
    let name = if let Some(end) = host.strip_prefix('[').and_then(|r| r.find(']')) {
        &host[1..=end]
    } else {
        host.split(':').next().unwrap_or(host)
    };

    let lowercase = name.to_ascii_lowercase();
    let allowed = name.parse::<std::net::IpAddr>().is_ok()
        || lowercase == "localhost"
        || lowercase.ends_with(".localhost")
        || lowercase.ends_with(".local")
        || config
            .hostname
            .as_deref()
            .is_some_and(|own| matches_hostname(&lowercase, own));

    allowed.then(|| host.to_string())
}

/// Whether a `Host` name is this machine's own, by either its fully qualified
/// or its short form.
fn matches_hostname(name: &str, hostname: &str) -> bool {
    let hostname = hostname.to_ascii_lowercase();
    if name == hostname {
        return true;
    }
    hostname.split('.').next() == Some(name)
}

/// Section 6.2: a request must say how long its body is.
fn declares_a_body(headers: &HeaderMap) -> bool {
    headers.contains_key(header::CONTENT_LENGTH) || headers.contains_key(header::TRANSFER_ENCODING)
}

/// Section 6.3: and what kind of body it is.
fn is_ipp(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case(IPP_CONTENT_TYPE)
        })
        .unwrap_or(false)
}

/// Serves an icon, honouring `If-Modified-Since`.
fn icon_response(
    icon: &icons::Icon,
    headers: &HeaderMap,
    modified: DateTime<Utc>,
    method: &Method,
) -> Response {
    // Second resolution is all HTTP dates carry, so compare at that.
    let modified = modified
        .with_timezone(&Utc)
        .with_nanosecond(0)
        .unwrap_or(modified);

    if let Some(since) = headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_http_date)
        && modified <= since
    {
        return (
            HttpStatus::NOT_MODIFIED,
            [(header::LAST_MODIFIED, format_http_date(modified))],
        )
            .into_response();
    }

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(Bytes::from_static(icon.bytes))
    };

    (
        HttpStatus::OK,
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (header::LAST_MODIFIED, format_http_date(modified)),
            (
                header::CACHE_CONTROL,
                format!("public, max-age={ICON_MAX_AGE}"),
            ),
        ],
        body,
    )
        .into_response()
}

/// A plain HTML page for the browsers.
fn status_page<B: Backend>(printer: &Printer<B>, method: &Method) -> Response {
    let config = printer.config();
    let page = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>{name}</title></head><body>\
         <h1>{name}</h1><p>{info}</p>\
         <p>IPP endpoint: <code>{path}</code></p>\
         </body></html>\n",
        name = escape(&config.name),
        info = escape(&config.info),
        path = escape(&config.path),
    );

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(page)
    };

    (
        HttpStatus::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn text(status: HttpStatus, message: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        message,
    )
        .into_response()
}

/// Formats an HTTP date (RFC 7231 IMF-fixdate).
fn format_http_date(at: DateTime<Utc>) -> String {
    at.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Parses the three date formats RFC 7231 says a Client may send.
fn parse_http_date(value: &str) -> Option<DateTime<Utc>> {
    use chrono::NaiveDateTime;

    let value = value.trim();
    for format in [
        // IMF-fixdate, which is what everything sends.
        "%a, %d %b %Y %H:%M:%S GMT",
        // The two obsolete forms, which a Printer must still accept.
        "%A, %d-%b-%y %H:%M:%S GMT",
        "%a %b %e %H:%M:%S %Y",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Some(naive.and_utc());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    fn config_with_hosts(hostname: Option<&str>) -> crate::PrinterConfig {
        crate::PrinterConfig {
            hostname: hostname.map(str::to_string),
            ..crate::PrinterConfig::default()
        }
    }

    #[test]
    fn addresses_localhost_and_mdns_names_are_accepted() {
        let config = config_with_hosts(None);
        for host in [
            "192.168.1.5:631",
            "127.0.0.1",
            "[::1]:631",
            "localhost:6631",
            "LOCALHOST",
            "printer.local",
            "printer.local:631",
        ] {
            assert!(
                validated_host(&headers(&[(header::HOST, host)]), &config).is_some(),
                "rejected {host}"
            );
        }
    }

    #[test]
    fn an_unknown_name_is_refused_but_this_machines_own_is_not() {
        let config = config_with_hosts(None);
        assert!(validated_host(&headers(&[(header::HOST, "evil.example.com")]), &config).is_none());
        // Missing entirely, as HTTP/1.0 clients do.
        assert!(validated_host(&HeaderMap::new(), &config).is_none());
        assert!(validated_host(&headers(&[(header::HOST, "")]), &config).is_none());

        // The machine's own name is accepted, fully qualified or short, and
        // whatever case the client used.
        let config = config_with_hosts(Some("printserver.example.com"));
        for host in [
            "PrintServer.Example.com:6631",
            "printserver.example.com",
            "printserver",
            "PRINTSERVER:6631",
        ] {
            assert!(
                validated_host(&headers(&[(header::HOST, host)]), &config).is_some(),
                "rejected {host}"
            );
        }
        // Still not somebody else's.
        assert!(validated_host(&headers(&[(header::HOST, "evil.example.com")]), &config).is_none());
    }

    #[test]
    fn the_host_is_returned_with_its_port_for_building_uris() {
        let config = config_with_hosts(None);
        assert_eq!(
            validated_host(&headers(&[(header::HOST, "10.0.0.1:6631")]), &config),
            Some("10.0.0.1:6631".to_string())
        );
    }

    #[test]
    fn a_body_must_be_declared_and_be_ipp() {
        assert!(declares_a_body(&headers(&[(header::CONTENT_LENGTH, "10")])));
        assert!(declares_a_body(&headers(&[(
            header::TRANSFER_ENCODING,
            "chunked"
        )])));
        assert!(!declares_a_body(&HeaderMap::new()));

        assert!(is_ipp(&headers(&[(
            header::CONTENT_TYPE,
            "application/ipp"
        )])));
        assert!(is_ipp(&headers(&[(
            header::CONTENT_TYPE,
            "Application/IPP; charset=utf-8"
        )])));
        assert!(!is_ipp(&headers(&[(
            header::CONTENT_TYPE,
            "application/octet-stream"
        )])));
        assert!(!is_ipp(&HeaderMap::new()));
    }

    #[test]
    fn http_dates_round_trip_and_the_obsolete_forms_parse() {
        let when = DateTime::parse_from_rfc3339("1994-11-06T08:49:37Z")
            .unwrap()
            .with_timezone(&Utc);
        let formatted = format_http_date(when);
        assert_eq!(formatted, "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(parse_http_date(&formatted), Some(when));

        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(when)
        );
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(when));
        assert_eq!(parse_http_date("not a date"), None);
    }

    /// The parts of a request, as the access log sees them.
    fn parts_of(method: Method, uri: &str, headers: &[(header::HeaderName, &str)]) -> Parts {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(name.clone(), *value);
        }
        builder.body(Body::empty()).unwrap().into_parts().0
    }

    fn summary_of_a_print_job() -> IppSummary {
        IppSummary {
            operation: Some(crate::Operations::PRINT_JOB),
            request_id: Some(4),
            status: Some(crate::StatusCode::SuccessfulOk),
            job_id: Some(12),
            job_name: Some("Report.pdf".into()),
            document_format: Some("application/pdf".into()),
            user: Some("alice".into()),
        }
    }

    #[test]
    fn an_access_line_carries_the_request_and_what_the_ipp_layer_made_of_it() {
        let parts = parts_of(
            Method::POST,
            "/ipp/print",
            &[
                (header::HOST, "printer.local:631"),
                (header::USER_AGENT, "CUPS/2.4.7 (Linux) IPP/2.0"),
            ],
        );
        let line = access_line(
            "192.0.2.10".parse().unwrap(),
            &parts,
            HttpStatus::OK,
            Some(&summary_of_a_print_job()),
            Duration::from_micros(3_100),
        );

        assert!(
            line.starts_with("192.0.2.10 \"POST /ipp/print HTTP/1.1\" 200 "),
            "{line}"
        );
        for piece in [
            "host=\"printer.local:631\"",
            "ua=\"CUPS/2.4.7 (Linux) IPP/2.0\"",
            "ipp=Print-Job",
            "ipp-status=successful-ok",
            "request-id=4",
            "job=12",
            "user=\"alice\"",
            "format=\"application/pdf\"",
            "job-name=\"Report.pdf\"",
        ] {
            assert!(line.contains(piece), "{piece} missing from {line}");
        }
    }

    #[test]
    fn a_request_that_never_reached_the_ipp_layer_is_logged_without_it() {
        let parts = parts_of(Method::GET, "/favicon.ico", &[]);
        let line = access_line(
            "198.51.100.3".parse().unwrap(),
            &parts,
            HttpStatus::NOT_FOUND,
            None,
            Duration::from_micros(80),
        );

        assert!(line.contains("\"GET /favicon.ico HTTP/1.1\" 404"), "{line}");
        // Nothing was sent, so nothing is claimed.
        assert!(line.contains("host=-"), "{line}");
        assert!(!line.contains("ua="), "{line}");
        assert!(!line.contains("ipp"), "{line}");
    }

    #[test]
    fn a_query_string_is_logged_and_an_unknown_operation_keeps_its_number() {
        let parts = parts_of(
            Method::POST,
            "/ipp/print?debug=1",
            &[(header::HOST, "[::1]")],
        );
        let summary = IppSummary {
            operation: Some(0x7FFF),
            status: Some(crate::StatusCode::ServerErrorOperationNotSupported),
            ..IppSummary::default()
        };
        let line = access_line(
            "::1".parse().unwrap(),
            &parts,
            HttpStatus::OK,
            Some(&summary),
            Duration::from_millis(1),
        );

        assert!(line.contains("/ipp/print?debug=1"), "{line}");
        assert!(line.contains("ipp=0x7fff"), "{line}");
        assert!(
            line.contains("ipp-status=server-error-operation-not-supported"),
            "{line}"
        );
    }

    #[test]
    fn a_client_cannot_write_a_line_of_its_own_into_the_log() {
        let parts = parts_of(
            Method::POST,
            "/ipp/print",
            &[(header::HOST, "printer.local")],
        );
        let summary = IppSummary {
            job_name: Some("sneaky\" 200 ipp=Print-Job\nnot a request".into()),
            ..IppSummary::default()
        };
        let line = access_line(
            "192.0.2.10".parse().unwrap(),
            &parts,
            HttpStatus::OK,
            Some(&summary),
            Duration::from_millis(1),
        );

        assert!(!line.contains('\n'), "{line}");
        assert!(
            line.contains(r#"job-name="sneaky\" 200 ipp=Print-Job\nnot a request""#),
            "{line}"
        );
    }

    #[test]
    fn an_overlong_value_is_cut_short() {
        let name: String = "a".repeat(MAX_LOGGED + 50);
        let quoted = quoted(&name);
        assert!(quoted.chars().count() < name.chars().count());
        assert!(quoted.ends_with("a\u{2026}\""), "{quoted}");
    }

    #[test]
    fn html_in_the_printer_name_cannot_escape_the_status_page() {
        assert_eq!(escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
