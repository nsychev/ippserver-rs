//! The operations and queue implementation.

use std::io::{Cursor, Read};
use std::net::IpAddr;
use std::sync::Arc;

use ipp::attribute::{IppAttribute, IppAttributeName};
use ipp::model::{DelimiterTag, IppVersion, JobState, PrinterState, StatusCode};
use ipp::parser::IppParser;
use ipp::request::IppRequestResponse;
use ipp::value::IppValue;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::Instant;

pub use bytes::Bytes;

use crate::attributes::{self, JOB_CREATION_ATTRIBUTES, PrinterContext};
use crate::job::{Clock, CreateError, Job, Moment, WHICH_JOBS};
use crate::{Backend, ConfigError, JobRequest, Operations, Outcome, PrinterConfig, Rejection};

/// IPP's out-of-band `unsupported` value tag, used in the Unsupported
/// Attributes group.
const TAG_UNSUPPORTED: u8 = 0x10;

/// The value tags the first two operation attributes must carry.
const TAG_CHARSET: u8 = 0x47;
const TAG_NATURAL_LANGUAGE: u8 = 0x48;

/// Where a request came from.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// The peer address.
    pub origin: IpAddr,
    /// The validated `Host` header, used to build absolute URIs.
    pub host: String,
}

/// Request metadata for access logs.
#[derive(Debug, Clone, Default)]
pub struct IppSummary {
    pub operation: Option<u16>,
    pub request_id: Option<i32>,
    pub status: Option<StatusCode>,
    pub job_id: Option<i32>,
    pub job_name: Option<String>,
    pub document_format: Option<String>,
    pub user: Option<String>,
}

impl IppSummary {
    /// The IPP name of the operation, like `Print-Job`, if it is one of the
    /// operations IPP defines.
    pub fn operation_name(&self) -> Option<String> {
        use ipp::prelude::*;

        Operation::from_u16(self.operation?).map(|known| hyphenate(&format!("{known:?}")))
    }

    /// The IPP keyword for the status, like `successful-ok`.
    pub fn status_name(&self) -> Option<String> {
        Some(hyphenate(&format!("{:?}", self.status?)).to_ascii_lowercase())
    }

    fn describe(&mut self, operation: u16, request: &IppRequestResponse) {
        self.operation = Some(operation);
        self.job_id = requested_job_id(request);
        self.job_name = name_or_text(operation_attribute(request, IppAttribute::JOB_NAME))
            .or_else(|| name_or_text(operation_attribute(request, IppAttribute::DOCUMENT_NAME)))
            .map(sanitise);
        self.user = name_or_text(operation_attribute(
            request,
            IppAttribute::REQUESTING_USER_NAME,
        ))
        .map(sanitise);
        self.document_format = declared_format(request);
    }
}

#[derive(Debug, Clone)]
pub struct Exchange {
    /// The encoded IPP response.
    pub response: Vec<u8>,
    pub summary: IppSummary,
}

/// A job waiting for the worker.
struct Submission<P> {
    request: JobRequest,
    plan: P,
}

/// An IPP Printer.
pub struct Printer<B: Backend> {
    config: PrinterConfig,
    clock: Clock,
    jobs: crate::job::JobStore,
    backend: B,
    queue: mpsc::UnboundedSender<Submission<B::Plan>>,
    started: Moment,
}

impl<B: Backend> Printer<B> {
    /// Starts a Printer and its worker.
    pub fn start(config: PrinterConfig, backend: B) -> Result<Arc<Self>, ConfigError> {
        config.validate()?;

        let clock = Clock::new();
        let started = Moment::now(&clock);
        let jobs = crate::job::JobStore::new(config.job_history, config.max_queued_per_origin);
        let (queue, receiver) = mpsc::unbounded_channel();

        let printer = Arc::new(Printer {
            config,
            clock,
            jobs,
            backend,
            queue,
            started,
        });

        tokio::spawn(worker(Arc::clone(&printer), receiver));
        Ok(printer)
    }

    pub fn config(&self) -> &PrinterConfig {
        &self.config
    }

    /// When the Printer started, used as the `Last-Modified` of its icons.
    pub fn started_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.started.at
    }

    /// The job store, for status reporting.
    pub fn jobs(&self) -> &crate::job::JobStore {
        &self.jobs
    }

    /// Describes the Printer current status.
    pub fn context<'a>(&'a self, host: &'a str) -> PrinterContext<'a> {
        PrinterContext {
            config: &self.config,
            host,
            uptime: self.clock.ticks(),
            queued: self.jobs.active_count(),
            state: if self.jobs.active_count() > 0 {
                PrinterState::Processing
            } else {
                PrinterState::Idle
            },
            state_change: self.started,
            config_change: self.started,
        }
    }

    /// Handles one IPP request, returning the encoded response and metadata.
    pub async fn handle(&self, body: Bytes, ctx: &RequestContext) -> Exchange {
        let mut summary = IppSummary::default();
        let response = self.respond_to(body, ctx, &mut summary).await;

        summary.status = Some(response.header().status_code());
        if summary.job_id.is_none()
            && matches!(
                summary.operation,
                Some(Operations::PRINT_JOB | Operations::CREATE_JOB)
            )
        {
            summary.job_id = created_job_id(&response);
        }

        Exchange {
            response: response.to_bytes().to_vec(),
            summary,
        }
    }

    async fn respond_to(
        &self,
        body: Bytes,
        ctx: &RequestContext,
        summary: &mut IppSummary,
    ) -> IppRequestResponse {
        let request_id = body
            .get(4..8)
            .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        summary.request_id = request_id;
        let request_id = request_id.unwrap_or(0);

        // RFC 8011 section 4.1.8: an unsupported version is refused before
        // anything else is looked at.
        let Some(version) = supported_version(&body) else {
            return self.error(
                IppVersion::v2_0(),
                request_id,
                StatusCode::ServerErrorVersionNotSupported,
                "this printer speaks IPP 1.0, 1.1 and 2.0",
            );
        };

        // RFC 8011 section 4.1.1: zero is not a request id.
        if request_id == 0 {
            return self.error(
                version,
                request_id,
                StatusCode::ClientErrorBadRequest,
                "request-id must not be 0",
            );
        }

        // RFC 8011 section 4.1.4: the first two operation attributes are
        // attributes-charset then attributes-natural-language, in that order.
        if !has_required_preamble(&body) {
            return self.error(
                version,
                request_id,
                StatusCode::ClientErrorBadRequest,
                "attributes-charset and attributes-natural-language must come first",
            );
        }

        let request = match IppParser::new(Cursor::new(body.clone())).parse() {
            Ok(request) => request,
            Err(error) => {
                return self.error(
                    version,
                    request_id,
                    StatusCode::ClientErrorBadRequest,
                    format!("malformed request: {error}"),
                );
            }
        };

        if let Some(charset) = operation_attribute(&request, IppAttribute::ATTRIBUTES_CHARSET)
            && let IppValue::Charset(charset) = charset
            && !charset.eq_ignore_ascii_case("utf-8")
        {
            return self.error(
                version,
                request_id,
                StatusCode::ClientErrorCharsetNotSupported,
                "only utf-8 is supported",
            );
        }

        // `operation_or_status` is the one signed 16-bit header field IPP
        // uses for an operation id in a request and a status code in a
        // response.
        let operation = request.header().operation_or_status as u16;
        summary.describe(operation, &request);
        self.dispatch(operation, request, version, request_id, ctx)
            .await
    }

    async fn dispatch(
        &self,
        operation: u16,
        request: IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> IppRequestResponse {
        // Every operation here is directed at the Printer, so all of them
        // need a printer-uri (RFC 8011 section 4.2).
        if operation_attribute(&request, IppAttribute::PRINTER_URI).is_none()
            && operation_attribute(&request, IppAttribute::JOB_URI).is_none()
        {
            return self.error(
                version,
                request_id,
                StatusCode::ClientErrorBadRequest,
                "printer-uri is required",
            );
        }

        let result = match operation {
            Operations::GET_PRINTER_ATTRIBUTES => {
                Ok(self.get_printer_attributes(&request, version, request_id, ctx))
            }
            Operations::IDENTIFY_PRINTER => {
                let message = match operation_attribute(&request, IppAttribute::MESSAGE) {
                    Some(IppValue::TextWithoutLanguage(text)) => Some(text.clone()),
                    _ => None,
                };
                self.backend.identify(message.as_deref()).await;
                Ok(self.ok(version, request_id))
            }
            Operations::VALIDATE_JOB => self.validate_job(&request, version, request_id),
            Operations::PRINT_JOB => self.print_job(request, version, request_id, ctx).await,
            Operations::CREATE_JOB => self.create_job(&request, version, request_id, ctx),
            Operations::SEND_DOCUMENT => {
                self.send_document(request, version, request_id, ctx).await
            }
            Operations::CLOSE_JOB => self.close_job(&request, version, request_id, ctx),
            Operations::CANCEL_JOB => self.cancel_job(&request, version, request_id),
            Operations::CANCEL_MY_JOBS => self.cancel_my_jobs(&request, version, request_id),
            Operations::GET_JOB_ATTRIBUTES => {
                self.get_job_attributes(&request, version, request_id, ctx)
            }
            Operations::GET_JOBS => Ok(self.get_jobs(&request, version, request_id, ctx)),
            _ => Err(Rejection::new(
                StatusCode::ServerErrorOperationNotSupported,
                format!("operation {operation:#06x} is not supported"),
            )),
        };

        match result {
            Ok(response) => response,
            Err(rejection) => self.error(version, request_id, rejection.status, rejection.message),
        }
    }

    // -- responses ---------------------------------------------------------

    fn ok(&self, version: IppVersion, request_id: i32) -> IppRequestResponse {
        self.respond(version, request_id, StatusCode::SuccessfulOk, None)
    }

    fn error(
        &self,
        version: IppVersion,
        request_id: i32,
        status: StatusCode,
        message: impl Into<String>,
    ) -> IppRequestResponse {
        self.respond(version, request_id, status, Some(message.into()))
    }

    fn respond(
        &self,
        version: IppVersion,
        request_id: i32,
        status: StatusCode,
        message: Option<String>,
    ) -> IppRequestResponse {
        // `new_response` puts `attributes-charset` and
        // `attributes-natural-language` in the operation group itself, in the
        // order RFC 8011 section 4.1.4 requires, so they are not added again
        // here. It is fallible only in principle: the values it builds are
        // its own literals.
        let mut response = IppRequestResponse::new_response(version, status, request_id)
            .expect("the charset and language of a response are constants");
        if let Some(message) = message {
            response.attributes_mut().add(
                DelimiterTag::OperationAttributes,
                IppAttribute::new(
                    IppAttributeName::STATUS_MESSAGE,
                    attributes::status_message(&message),
                ),
            );
        }
        response
    }

    /// Adds a job's attributes to a response.
    fn add_job_attributes(
        &self,
        response: &mut IppRequestResponse,
        ctx: &RequestContext,
        job: &Job,
        requested: Option<&[String]>,
    ) {
        let printer = self.context(&ctx.host);
        let intervening = self.jobs.intervening(job.id);
        for attribute in attributes::job_attributes(&printer, job, intervening) {
            if wanted(requested, attribute.name()) {
                response
                    .attributes_mut()
                    .add(DelimiterTag::JobAttributes, attribute);
            }
        }
    }

    // -- operations --------------------------------------------------------

    fn get_printer_attributes(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> IppRequestResponse {
        let requested = requested_attributes(request);
        let mut response = self.ok(version, request_id);
        let printer = self.context(&ctx.host);

        for attribute in attributes::printer_attributes(&printer) {
            if wanted(requested.as_deref(), attribute.name()) {
                response
                    .attributes_mut()
                    .add(DelimiterTag::PrinterAttributes, attribute);
            }
        }
        response
    }

    fn validate_job(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
    ) -> Result<IppRequestResponse, Rejection> {
        self.check_document_format(request)?;
        self.check_compression(request)?;

        let (_, unsupported) = self.split_template(request);
        let fidelity = fidelity(request);
        if fidelity && !unsupported.is_empty() {
            return Err(Rejection::new(
                StatusCode::ClientErrorConflictingAttributes,
                "ipp-attribute-fidelity is true and some attributes are not supported",
            ));
        }

        let status = if unsupported.is_empty() {
            StatusCode::SuccessfulOk
        } else {
            StatusCode::SuccessfulOkIgnoredOrSubstitutedAttributes
        };
        let mut response = self.respond(version, request_id, status, None);
        add_unsupported(&mut response, &unsupported);
        Ok(response)
    }

    async fn print_job(
        &self,
        request: IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> Result<IppRequestResponse, Rejection> {
        self.check_document_format(&request)?;
        self.check_compression(&request)?;

        let fidelity = fidelity(&request);
        let (template, unsupported) = self.split_template(&request);
        if fidelity && !unsupported.is_empty() {
            return Err(Rejection::new(
                StatusCode::ClientErrorConflictingAttributes,
                "ipp-attribute-fidelity is true and some attributes are not supported",
            ));
        }

        let name = job_name(&request);
        let user = requesting_user(&request);
        let format = declared_format(&request)
            .unwrap_or_else(|| self.config.default_document_format.clone());
        let compression = compression(&request);

        let document = self.read_document(request, compression.as_deref())?;

        let job = self.create_job_object(ctx, user, name, format, template)?;
        let response = self
            .accept_document(job, document, version, request_id, ctx, &unsupported)
            .await?;
        Ok(response)
    }

    fn create_job(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> Result<IppRequestResponse, Rejection> {
        self.check_document_format(request)?;

        let fidelity = fidelity(request);
        let (template, unsupported) = self.split_template(request);
        if fidelity && !unsupported.is_empty() {
            return Err(Rejection::new(
                StatusCode::ClientErrorConflictingAttributes,
                "ipp-attribute-fidelity is true and some attributes are not supported",
            ));
        }

        let job = self.create_job_object(
            ctx,
            requesting_user(request),
            job_name(request),
            declared_format(request).unwrap_or_else(|| self.config.default_document_format.clone()),
            template,
        )?;

        let status = if unsupported.is_empty() {
            StatusCode::SuccessfulOk
        } else {
            StatusCode::SuccessfulOkIgnoredOrSubstitutedAttributes
        };
        let mut response = self.respond(version, request_id, status, None);
        add_unsupported(&mut response, &unsupported);
        self.add_job_attributes(&mut response, ctx, &job, None);
        Ok(response)
    }

    async fn send_document(
        &self,
        request: IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> Result<IppRequestResponse, Rejection> {
        let id = self.job_id(&request)?;
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| Rejection::new(StatusCode::ClientErrorNotFound, "no such job"))?;

        // RFC 8011 section 4.3.1 makes last-document REQUIRED.
        let Some(last) = last_document(&request) else {
            return Err(Rejection::new(
                StatusCode::ClientErrorBadRequest,
                "last-document is required",
            ));
        };

        if job.has_document {
            return Err(Rejection::new(
                StatusCode::ServerErrorMultipleDocumentJobsNotSupported,
                "this printer accepts one document per job",
            ));
        }
        if job.is_finished() {
            return Err(Rejection::new(
                StatusCode::ClientErrorNotPossible,
                "the job has already finished",
            ));
        }

        self.check_document_format(&request)?;
        self.check_compression(&request)?;
        let compression = compression(&request);
        let document = self.read_document(request, compression.as_deref())?;

        if !last {
            // `multiple-document-jobs-supported` is false, so there is no
            // honest way to accept a document that promises more to come.
            return Err(Rejection::new(
                StatusCode::ServerErrorMultipleDocumentJobsNotSupported,
                "this printer accepts one document per job; set last-document to true",
            ));
        }

        self.accept_document(job, document, version, request_id, ctx, &[])
            .await
    }

    fn close_job(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> Result<IppRequestResponse, Rejection> {
        let id = self.job_id(request)?;
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| Rejection::new(StatusCode::ClientErrorNotFound, "no such job"))?;

        // Closing a job that never received a document leaves nothing to
        // print, so it ends there rather than sitting in the queue for ever.
        let job = if !job.has_document && job.is_active() {
            self.jobs
                .update(id, |j| {
                    j.finish(
                        &self.clock,
                        JobState::Aborted,
                        "job-aborted-by-system",
                        "closed without a document",
                    )
                })
                .unwrap_or(job)
        } else {
            job
        };

        let mut response = self.ok(version, request_id);
        self.add_job_attributes(&mut response, ctx, &job, None);
        Ok(response)
    }

    fn cancel_job(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
    ) -> Result<IppRequestResponse, Rejection> {
        let id = self.job_id(request)?;
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| Rejection::new(StatusCode::ClientErrorNotFound, "no such job"))?;

        if job.is_finished() {
            return Err(Rejection::new(
                StatusCode::ClientErrorNotPossible,
                "the job has already finished",
            ));
        }

        self.cancel(id);
        Ok(self.ok(version, request_id))
    }

    fn cancel_my_jobs(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
    ) -> Result<IppRequestResponse, Rejection> {
        let user = requesting_user(request);
        for job in self.jobs.list("not-completed", Some(&user), None) {
            self.cancel(job.id);
        }
        Ok(self.ok(version, request_id))
    }

    fn get_job_attributes(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> Result<IppRequestResponse, Rejection> {
        let id = self.job_id(request)?;
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| Rejection::new(StatusCode::ClientErrorNotFound, "no such job"))?;

        let requested = requested_attributes(request);
        let mut response = self.ok(version, request_id);
        self.add_job_attributes(&mut response, ctx, &job, requested.as_deref());
        Ok(response)
    }

    fn get_jobs(
        &self,
        request: &IppRequestResponse,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
    ) -> IppRequestResponse {
        let which = match operation_attribute(request, IppAttribute::WHICH_JOBS) {
            Some(IppValue::Keyword(value)) if WHICH_JOBS.contains(&value.as_str()) => {
                value.as_str().to_string()
            }
            _ => "not-completed".to_string(),
        };
        let mine = matches!(
            operation_attribute(request, IppAttribute::MY_JOBS),
            Some(IppValue::Boolean(true))
        );
        let user = requesting_user(request);
        let limit = match operation_attribute(request, IppAttribute::LIMIT) {
            Some(IppValue::Integer(n)) if *n > 0 => Some(*n as usize),
            _ => None,
        };
        // Unlike Get-Job-Attributes, Get-Jobs returns only the two
        // identifying attributes unless the Client asks for more
        // (RFC 8011 section 4.2.6.1).
        let requested = requested_attributes(request).unwrap_or_else(|| {
            vec![
                IppAttribute::JOB_URI.to_string(),
                IppAttribute::JOB_ID.to_string(),
            ]
        });

        let mut response = self.ok(version, request_id);
        let jobs = self.jobs.list(&which, mine.then_some(user.as_str()), limit);

        let printer = self.context(&ctx.host);
        for job in &jobs {
            // Each job goes in its own group, so a Client can tell them apart.
            let intervening = self.jobs.intervening(job.id);
            let group = attributes::job_attributes(&printer, job, intervening)
                .into_iter()
                .filter(|a| wanted(Some(&requested), a.name()))
                .collect::<Vec<_>>();
            push_group(&mut response, DelimiterTag::JobAttributes, group);
        }
        response
    }

    // -- helpers -----------------------------------------------------------

    /// Creates the Job object, mapping a full queue onto an IPP status.
    fn create_job_object(
        &self,
        ctx: &RequestContext,
        user: String,
        name: String,
        format: String,
        template: Vec<IppAttribute>,
    ) -> Result<Job, Rejection> {
        self.jobs
            .create(&self.clock, ctx.origin, user, name, format, template)
            .map_err(|CreateError::TooManyJobs { queued, limit }| {
                // 'server-error-busy' tells the Client to try again later,
                // which is exactly right: the limit is about pacing one
                // client, not about refusing it for ever.
                Rejection::new(
                    StatusCode::ServerErrorBusy,
                    format!(
                        "{queued} of {limit} jobs from {} are still queued",
                        ctx.origin
                    ),
                )
            })
    }

    /// Hands a received document to the backend and queues the job.
    async fn accept_document(
        &self,
        job: Job,
        document: Vec<u8>,
        version: IppVersion,
        request_id: i32,
        ctx: &RequestContext,
        unsupported: &[IppAttribute],
    ) -> Result<IppRequestResponse, Rejection> {
        let k_octets = document.len().div_ceil(1024) as i32;
        let mut request = self.job_request(&job, document);

        let accepted = match self.backend.validate(&request).await {
            Ok(accepted) => accepted,
            Err(rejection) => {
                // The job exists, so it has to be accounted for rather than
                // simply forgotten.
                self.jobs.update(job.id, |j| {
                    j.finish(
                        &self.clock,
                        JobState::Aborted,
                        "job-aborted-by-system",
                        &rejection.message,
                    )
                });
                return Err(rejection);
            }
        };

        if let Some(limit) = self.config.max_pages_per_job
            && accepted.impressions > limit
        {
            let message = format!(
                "the job has {} pages and the limit is {limit}",
                accepted.impressions
            );
            self.jobs.update(job.id, |j| {
                j.finish(
                    &self.clock,
                    JobState::Aborted,
                    "job-aborted-by-system",
                    &message,
                )
            });
            return Err(Rejection::new(
                StatusCode::ClientErrorAttributesOrValuesNotSupported,
                message,
            ));
        }

        if let Some(format) = accepted.format.clone() {
            request.format = format.clone();
            self.jobs.update(job.id, |j| j.document_format = format);
        }
        let job = self
            .jobs
            .update(job.id, |j| {
                j.impressions = accepted.impressions;
                j.k_octets = k_octets;
                j.has_document = true;
            })
            .unwrap_or(job);

        self.queue
            .send(Submission {
                request,
                plan: accepted.plan,
            })
            .map_err(|_| {
                Rejection::new(
                    StatusCode::ServerErrorServiceUnavailable,
                    "the print queue is not running",
                )
            })?;

        let status = if unsupported.is_empty() {
            StatusCode::SuccessfulOk
        } else {
            StatusCode::SuccessfulOkIgnoredOrSubstitutedAttributes
        };
        let mut response = self.respond(version, request_id, status, None);
        add_unsupported(&mut response, unsupported);
        self.add_job_attributes(&mut response, ctx, &job, None);
        Ok(response)
    }

    fn job_request(&self, job: &Job, document: Vec<u8>) -> JobRequest {
        JobRequest {
            id: job.id,
            uuid: job.uuid,
            name: job.name.clone(),
            user: job.user.clone(),
            origin: job.origin,
            format: job.document_format.clone(),
            document,
            template: job.template.clone(),
        }
    }

    /// Cancels a job, or marks it for the worker to drop.
    fn cancel(&self, id: i32) {
        self.jobs.update(id, |job| {
            job.cancel_requested = true;
            if job.state == JobState::Pending || job.state == JobState::PendingHeld {
                job.finish(
                    &self.clock,
                    JobState::Canceled,
                    "job-canceled-by-user",
                    "canceled before printing",
                );
            }
        });
    }

    fn job_id(&self, request: &IppRequestResponse) -> Result<i32, Rejection> {
        requested_job_id(request).ok_or_else(|| {
            Rejection::new(
                StatusCode::ClientErrorBadRequest,
                "job-id or job-uri is required",
            )
        })
    }

    fn check_document_format(&self, request: &IppRequestResponse) -> Result<(), Rejection> {
        let Some(format) = declared_format(request) else {
            return Ok(());
        };
        // "application/octet-stream" means "guess it from the content".
        if format == "application/octet-stream" || self.config.document_formats.contains(&format) {
            return Ok(());
        }
        Err(Rejection::new(
            StatusCode::ClientErrorDocumentFormatNotSupported,
            format!("{format} is not supported"),
        ))
    }

    fn check_compression(&self, request: &IppRequestResponse) -> Result<(), Rejection> {
        match compression(request).as_deref() {
            None | Some("none") | Some("gzip") | Some("deflate") => Ok(()),
            Some(other) => Err(Rejection::new(
                StatusCode::ClientErrorCompressionNotSupported,
                format!("{other} compression is not supported"),
            )),
        }
    }

    /// Reads the document out of a request, decompressing it if needed.
    fn read_document(
        &self,
        request: IppRequestResponse,
        compression: Option<&str>,
    ) -> Result<Vec<u8>, Rejection> {
        let mut payload = request.into_payload();
        let mut raw = Vec::new();
        // The transport has already bounded the body, so this cannot run away.
        payload.read_to_end(&mut raw).map_err(|e| {
            Rejection::new(
                StatusCode::ClientErrorDocumentAccessError,
                format!("could not read the document: {e}"),
            )
        })?;

        let decompressed = match compression {
            Some("gzip") => decompress_gzip(&raw),
            Some("deflate") => decompress_deflate(&raw),
            _ => return Ok(raw),
        };

        decompressed.map_err(|e| {
            Rejection::new(
                StatusCode::ClientErrorCompressionError,
                format!("could not decompress the document: {e}"),
            )
        })
    }

    /// Splits the Job Template group into what is honoured and what is not.
    fn split_template(
        &self,
        request: &IppRequestResponse,
    ) -> (Vec<IppAttribute>, Vec<IppAttribute>) {
        let mut accepted = Vec::new();
        let mut unsupported = Vec::new();

        let groups = request.attributes().groups_of(DelimiterTag::JobAttributes);
        for group in groups {
            for attribute in group.attributes() {
                if self.is_supported_template(attribute) {
                    accepted.push(attribute.clone());
                } else {
                    unsupported.push(IppAttribute::new(
                        attribute.name().clone(),
                        IppValue::Other {
                            tag: TAG_UNSUPPORTED,
                            data: Default::default(),
                        },
                    ));
                }
            }
        }
        (accepted, unsupported)
    }

    fn is_supported_template(&self, attribute: &IppAttribute) -> bool {
        if !JOB_CREATION_ATTRIBUTES.contains(&attribute.name().as_str()) {
            return false;
        }
        match (attribute.name().as_str(), attribute.value()) {
            (IppAttribute::COPIES, IppValue::Integer(n)) => *n >= 1 && *n <= self.config.max_copies,
            (
                IppAttribute::MEDIA,
                IppValue::Keyword(name) | IppValue::NameWithoutLanguage(name),
                // Only a sheet this Printer advertises. Accepting any size
                // the PWG defines would mean honouring one that is not in
                // `media-supported`.
            ) => self.config.media.iter().any(|m| m.name == name.as_str()),
            (IppAttribute::SIDES, IppValue::Keyword(value)) => matches!(
                value.as_str(),
                "one-sided" | "two-sided-long-edge" | "two-sided-short-edge"
            ),
            ("print-color-mode", IppValue::Keyword(value)) => {
                self.config.color_modes().contains(&value.as_str())
            }
            _ => true,
        }
    }
}

struct Queued<P> {
    submission: Submission<P>,
    ready: Instant,
    attempts: u32,
}

impl<P> Queued<P> {
    /// A job that has just arrived and may go at once.
    fn fresh(submission: Submission<P>) -> Self {
        Queued {
            submission,
            ready: Instant::now(),
            attempts: 0,
        }
    }
}

/// Prints queued jobs, one at a time.
///
/// Deferred jobs are deprioritized comparing to newer ones.
async fn worker<B: Backend>(
    printer: Arc<Printer<B>>,
    mut queue: mpsc::UnboundedReceiver<Submission<B::Plan>>,
) {
    let mut deferred: Vec<Queued<B::Plan>> = Vec::new();

    while let Some(queued) = next(&mut queue, &mut deferred).await {
        if let Some(again) = attempt(&printer, queued).await {
            deferred.push(again);
        }
    }
}

/// Poll for the next job to offer.
///
/// Returns `None` once no Client can submit again and nothing is deferred.
async fn next<P>(
    queue: &mut mpsc::UnboundedReceiver<Submission<P>>,
    deferred: &mut Vec<Queued<P>>,
) -> Option<Queued<P>> {
    let mut closed = false;

    loop {
        if !closed {
            match queue.try_recv() {
                Ok(submission) => return Some(Queued::fresh(submission)),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => closed = true,
            }
        }

        // No newer jobs, try deferred ones.
        let now = Instant::now();
        if let Some(index) = deferred
            .iter()
            .enumerate()
            .filter(|(_, job)| job.ready <= now)
            .min_by_key(|(_, job)| job.ready)
            .map(|(index, _)| index)
        {
            return Some(deferred.remove(index));
        }

        match deferred.iter().map(|job| job.ready).min() {
            // Wait for the first deferred job's turn, unless work arrives.
            Some(turn) if !closed => {
                tokio::select! {
                    biased;
                    received = queue.recv() => match received {
                        Some(submission) => return Some(Queued::fresh(submission)),
                        None => closed = true,
                    },
                    _ = tokio::time::sleep_until(turn) => {}
                }
            }
            Some(turn) => tokio::time::sleep_until(turn).await,
            None if closed => return None,
            None => match queue.recv().await {
                Some(submission) => return Some(Queued::fresh(submission)),
                None => closed = true,
            },
        }
    }
}

/// Offers one job to the backend, returning it if it should be offered again.
async fn attempt<B: Backend>(
    printer: &Arc<Printer<B>>,
    mut queued: Queued<B::Plan>,
) -> Option<Queued<B::Plan>> {
    let id = queued.submission.request.id;

    let job = printer.jobs.get(id)?;
    if job.cancel_requested || job.is_finished() {
        printer.jobs.update(id, |j| {
            if j.is_active() {
                j.finish(
                    &printer.clock,
                    JobState::Canceled,
                    "job-canceled-by-user",
                    "canceled before printing",
                );
            }
        });
        return None;
    }

    printer.jobs.update(id, |j| {
        j.state = JobState::Processing;
        j.reasons = vec!["job-printing".into()];
        j.processing = Some(Moment::now(&printer.clock));
    });

    queued.attempts += 1;
    match printer
        .backend
        .print(
            &queued.submission.request,
            &queued.submission.plan,
            queued.attempts,
        )
        .await
    {
        Outcome::Printed => {
            printer.jobs.update(id, |j| {
                j.finish(
                    &printer.clock,
                    JobState::Completed,
                    "job-completed-successfully",
                    "",
                )
            });
            tracing::info!(job = id, "printed");
            None
        }
        Outcome::Failed(error) => {
            tracing::error!(job = id, %error, "job failed");
            printer.jobs.update(id, |j| {
                j.finish(
                    &printer.clock,
                    JobState::Aborted,
                    "job-aborted-by-system",
                    &error,
                )
            });
            None
        }
        Outcome::Deferred { reason, after } => {
            tracing::warn!(
                job = id,
                attempt = queued.attempts,
                %reason,
                "deferred, will try again in {after:?}"
            );
            printer.jobs.update(id, |j| {
                j.state = JobState::Pending;
                j.reasons = vec!["resources-are-not-ready".into()];
                j.message = reason.clone();
                j.processing = None;
            });
            queued.ready = Instant::now() + after;
            Some(queued)
        }
    }
}

// ---------------------------------------------------------------------------
// Request inspection
// ---------------------------------------------------------------------------

fn supported_version(body: &[u8]) -> Option<IppVersion> {
    match (body.first()?, body.get(1)?) {
        (1, 0) => Some(IppVersion::v1_0()),
        (1, 1) => Some(IppVersion::v1_1()),
        (2, 0) => Some(IppVersion::v2_0()),
        _ => None,
    }
}

/// Whether the first two operation attributes are the required pair, in order.
///
/// The parsed form cannot answer this — `ipp` keeps attributes in a hash map —
/// so it is read straight off the wire, where the order is still visible.
fn has_required_preamble(body: &[u8]) -> bool {
    fn check(body: &[u8]) -> Option<bool> {
        if *body.get(8)? != DelimiterTag::OperationAttributes as u8 {
            return Some(false);
        }

        let mut pos = 9usize;
        for (tag, name) in [
            (TAG_CHARSET, IppAttribute::ATTRIBUTES_CHARSET),
            (TAG_NATURAL_LANGUAGE, "attributes-natural-language"),
        ] {
            if *body.get(pos)? != tag {
                return Some(false);
            }
            pos += 1;

            let name_len = u16::from_be_bytes([*body.get(pos)?, *body.get(pos + 1)?]) as usize;
            pos += 2;
            if body.get(pos..pos + name_len)? != name.as_bytes() {
                return Some(false);
            }
            pos += name_len;

            let value_len = u16::from_be_bytes([*body.get(pos)?, *body.get(pos + 1)?]) as usize;
            pos += 2 + value_len;
        }
        Some(true)
    }

    check(body).unwrap_or(false)
}

fn operation_attribute<'a>(request: &'a IppRequestResponse, name: &str) -> Option<&'a IppValue> {
    request
        .attributes()
        .groups_of(DelimiterTag::OperationAttributes)
        .next()?
        .get(name)
        .map(IppAttribute::value)
}

/// The job a request names, by `job-id` or by `job-uri`.
fn requested_job_id(request: &IppRequestResponse) -> Option<i32> {
    if let Some(IppValue::Integer(id)) = operation_attribute(request, IppAttribute::JOB_ID) {
        return Some(*id);
    }
    // A job-uri ends in the job id.
    if let Some(IppValue::Uri(uri)) = operation_attribute(request, IppAttribute::JOB_URI) {
        return uri.rsplit('/').next().and_then(|s| s.parse().ok());
    }
    None
}

/// The `job-id` of the job a response describes.
fn created_job_id(response: &IppRequestResponse) -> Option<i32> {
    match response
        .attributes()
        .groups_of(DelimiterTag::JobAttributes)
        .next()?
        .get(IppAttribute::JOB_ID)
        .map(IppAttribute::value)?
    {
        IppValue::Integer(id) => Some(*id),
        _ => None,
    }
}

/// `PrintJob` becomes `Print-Job`: the spelling IPP uses for the names of its
/// operations and, lowercased, of its status codes.
fn hyphenate(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in characters.iter().enumerate() {
        // A capital starts a new word unless it is in the middle of an
        // acronym, so `CupsGetPPD` keeps its `PPD` together.
        let starts_word = i > 0
            && c.is_ascii_uppercase()
            && (characters[i - 1].is_lowercase()
                || characters
                    .get(i + 1)
                    .is_some_and(|next| next.is_lowercase()));
        if starts_word {
            out.push('-');
        }
        out.push(c);
    }
    out
}

fn declared_format(request: &IppRequestResponse) -> Option<String> {
    match operation_attribute(request, IppAttribute::DOCUMENT_FORMAT)? {
        IppValue::MimeMediaType(format) => Some(format.split(';').next()?.trim().to_lowercase()),
        _ => None,
    }
}

fn compression(request: &IppRequestResponse) -> Option<String> {
    match operation_attribute(request, IppAttribute::COMPRESSION)? {
        IppValue::Keyword(value) => Some(value.as_str().to_string()),
        _ => None,
    }
}

fn fidelity(request: &IppRequestResponse) -> bool {
    matches!(
        operation_attribute(request, IppAttribute::IPP_ATTRIBUTE_FIDELITY),
        Some(IppValue::Boolean(true))
    )
}

fn last_document(request: &IppRequestResponse) -> Option<bool> {
    match operation_attribute(request, IppAttribute::LAST_DOCUMENT)? {
        IppValue::Boolean(value) => Some(*value),
        _ => None,
    }
}

/// The string inside a `name` or a `text` value, whichever the Client used.
///
/// The two are separate types in `ipp`, since IPP gives them different
/// maximum lengths, but a Client may send either for the attributes below.
fn name_or_text(value: Option<&IppValue>) -> Option<&str> {
    match value? {
        IppValue::NameWithoutLanguage(name) => Some(name.as_str()),
        IppValue::TextWithoutLanguage(text) => Some(text.as_ref()),
        _ => None,
    }
}

fn job_name(request: &IppRequestResponse) -> String {
    name_or_text(operation_attribute(request, IppAttribute::JOB_NAME))
        .or_else(|| name_or_text(operation_attribute(request, IppAttribute::DOCUMENT_NAME)))
        .map_or_else(|| "Untitled".to_string(), sanitise)
}

fn requesting_user(request: &IppRequestResponse) -> String {
    name_or_text(operation_attribute(
        request,
        IppAttribute::REQUESTING_USER_NAME,
    ))
    .map_or_else(|| "anonymous".to_string(), sanitise)
}

/// Strips the control characters IPP Everywhere section 8 forbids in returned
/// name and text values.
fn sanitise(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        // IPP name values are limited to 255 octets; truncate on a character
        // boundary so the result stays well-formed UTF-8.
        trimmed.chars().take(255).collect()
    }
}

/// The `requested-attributes` list, if the Client sent one.
fn requested_attributes(request: &IppRequestResponse) -> Option<Vec<String>> {
    let value = operation_attribute(request, IppAttribute::REQUESTED_ATTRIBUTES)?;
    let names: Vec<String> = value
        .into_iter()
        .filter_map(|v| match v {
            IppValue::Keyword(name) => Some(name.as_str().to_string()),
            _ => None,
        })
        .collect();
    (!names.is_empty()).then_some(names)
}

/// Whether an attribute was asked for.
fn wanted(requested: Option<&[String]>, name: &str) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    requested.iter().any(|r| {
        r == IppAttribute::ALL
            || r == name
            // Group names are treated as "everything in that group"; this
            // Printer has no attribute that belongs to neither.
            || r == IppAttribute::PRINTER_DESCRIPTION
            || r == IppAttribute::JOB_TEMPLATE
            || r == IppAttribute::JOB_DESCRIPTION
    })
}

fn add_unsupported(response: &mut IppRequestResponse, unsupported: &[IppAttribute]) {
    for attribute in unsupported {
        response
            .attributes_mut()
            .add(DelimiterTag::UnsupportedAttributes, attribute.clone());
    }
}

/// Adds a fresh group, so repeated groups of the same tag stay separate.
fn push_group(response: &mut IppRequestResponse, tag: DelimiterTag, attributes: Vec<IppAttribute>) {
    let mut group = ipp::attribute::IppAttributeGroup::new(tag);
    // A group holds its attributes in order now, rather than keyed by name,
    // so the ones built above are kept as they were produced.
    group.attributes_mut().extend(attributes);
    response.attributes_mut().groups_mut().push(group);
}

fn decompress_gzip(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(raw).read_to_end(&mut out)?;
    Ok(out)
}

/// IPP's `deflate` is zlib-wrapped in practice, but raw deflate turns up too,
/// so both are tried before giving up.
fn decompress_deflate(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    match flate2::read::ZlibDecoder::new(raw).read_to_end(&mut out) {
        Ok(_) => Ok(out),
        Err(zlib_error) => {
            out.clear();
            flate2::read::DeflateDecoder::new(raw)
                .read_to_end(&mut out)
                .map(|_| out)
                .map_err(|_| zlib_error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::test_support::test_config;
    use crate::{Accepted, Rejection};

    /// A backend that answers with the outcomes it was given, in order, and
    /// records which job it was offered on which attempt.
    struct Scripted {
        outcomes: Mutex<VecDeque<Outcome>>,
        offers: Mutex<Vec<(i32, u32)>>,
    }

    impl Scripted {
        fn new(outcomes: Vec<Outcome>) -> Arc<Self> {
            Arc::new(Scripted {
                outcomes: Mutex::new(outcomes.into()),
                offers: Mutex::new(Vec::new()),
            })
        }

        fn offers(&self) -> Vec<(i32, u32)> {
            self.offers.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Backend for Arc<Scripted> {
        type Plan = ();

        async fn validate(&self, _: &JobRequest) -> Result<Accepted<()>, Rejection> {
            Ok(Accepted {
                impressions: 1,
                format: None,
                plan: (),
            })
        }

        async fn print(&self, request: &JobRequest, _: &(), attempt: u32) -> Outcome {
            self.offers.lock().unwrap().push((request.id, attempt));
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Outcome::Printed)
        }
    }

    fn deferred(after: Duration) -> Outcome {
        Outcome::Deferred {
            reason: "out of paper".into(),
            after,
        }
    }

    /// Starts a Printer whose backend answers with `outcomes`.
    fn printer_with(outcomes: Vec<Outcome>) -> (Arc<Printer<Arc<Scripted>>>, Arc<Scripted>) {
        let backend = Scripted::new(outcomes);
        let printer = Printer::start(test_config(), Arc::clone(&backend)).unwrap();
        (printer, backend)
    }

    /// Queues a job, as `accept_document` does once a document has arrived.
    fn submit<B: Backend<Plan = ()>>(printer: &Arc<Printer<B>>) -> i32 {
        let job = printer
            .jobs
            .create(
                &printer.clock,
                std::net::Ipv4Addr::new(10, 0, 0, 1).into(),
                "tester".into(),
                "a job".into(),
                "application/pdf".into(),
                Vec::new(),
            )
            .expect("the store should take the job");
        printer.jobs.update(job.id, |j| j.has_document = true);

        let request = printer.job_request(&job, b"%PDF-1.7".to_vec());
        printer
            .queue
            .send(Submission { request, plan: () })
            .expect("the worker should be running");
        job.id
    }

    /// Lets the worker run until it is waiting on something.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_deferred_job_is_offered_again_and_prints() {
        let (printer, backend) = printer_with(vec![deferred(Duration::from_secs(30))]);
        let id = submit(&printer);

        settle().await;
        // Still the Printer's job, not a failure: a Client sees it waiting.
        let job = printer.jobs.get(id).unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.reasons, vec!["resources-are-not-ready".to_string()]);
        assert_eq!(job.message, "out of paper");
        assert!(job.is_active(), "a deferred job must still count as queued");

        // Once the wait is over it goes again, and the backend is told which
        // attempt this is.
        tokio::time::sleep(Duration::from_secs(60)).await;
        settle().await;
        assert_eq!(printer.jobs.get(id).unwrap().state, JobState::Completed);
        assert_eq!(backend.offers(), vec![(id, 1), (id, 2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_job_that_arrives_later_does_not_wait_behind_a_deferred_one() {
        // The first job is deferred for a minute; the second must not sit
        // behind it for that minute.
        let (printer, backend) = printer_with(vec![deferred(Duration::from_secs(60))]);
        let first = submit(&printer);
        let second = submit(&printer);

        settle().await;
        assert_eq!(
            printer.jobs.get(second).unwrap().state,
            JobState::Completed,
            "the newer job should have gone straight through"
        );
        assert_eq!(printer.jobs.get(first).unwrap().state, JobState::Pending);
        assert_eq!(backend.offers(), vec![(first, 1), (second, 1)]);

        // The deferred one follows once its turn comes and the queue is quiet.
        tokio::time::sleep(Duration::from_secs(90)).await;
        settle().await;
        assert_eq!(printer.jobs.get(first).unwrap().state, JobState::Completed);
        assert_eq!(backend.offers(), vec![(first, 1), (second, 1), (first, 2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deferred_job_can_be_cancelled_while_it_waits() {
        let (printer, backend) = printer_with(vec![deferred(Duration::from_secs(30))]);
        let id = submit(&printer);
        settle().await;

        printer.cancel(id);
        assert_eq!(printer.jobs.get(id).unwrap().state, JobState::Canceled);

        // Its turn comes and goes without the document being printed.
        tokio::time::sleep(Duration::from_secs(60)).await;
        settle().await;
        assert_eq!(printer.jobs.get(id).unwrap().state, JobState::Canceled);
        assert_eq!(backend.offers(), vec![(id, 1)], "it printed after cancel");
    }

    #[tokio::test(start_paused = true)]
    async fn a_backend_that_gives_up_aborts_the_job() {
        let (printer, backend) = printer_with(vec![
            deferred(Duration::from_secs(5)),
            Outcome::Failed("the printer is still out of paper".into()),
        ]);
        let id = submit(&printer);

        tokio::time::sleep(Duration::from_secs(30)).await;
        settle().await;

        let job = printer.jobs.get(id).unwrap();
        assert_eq!(job.state, JobState::Aborted);
        assert_eq!(job.message, "the printer is still out of paper");
        assert_eq!(backend.offers(), vec![(id, 1), (id, 2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn deferring_does_not_lose_the_job_while_others_are_printed() {
        // One sick job, a stream of healthy ones: the sick one is still
        // there at the end, and the healthy ones were not held up.
        let (printer, backend) = printer_with(vec![deferred(Duration::from_secs(45))]);
        let sick = submit(&printer);
        settle().await;

        let healthy: Vec<i32> = (0..3).map(|_| submit(&printer)).collect();
        settle().await;
        for id in &healthy {
            assert_eq!(printer.jobs.get(*id).unwrap().state, JobState::Completed);
        }
        assert_eq!(printer.jobs.get(sick).unwrap().state, JobState::Pending);

        tokio::time::sleep(Duration::from_secs(60)).await;
        settle().await;
        assert_eq!(printer.jobs.get(sick).unwrap().state, JobState::Completed);
        assert_eq!(backend.offers().last(), Some(&(sick, 2)));
    }

    #[test]
    fn versions_outside_the_supported_set_are_rejected() {
        let body = |major: u8, minor: u8| vec![major, minor, 0, 0x0B, 0, 0, 0, 1];
        assert!(supported_version(&body(1, 1)).is_some());
        assert!(supported_version(&body(1, 0)).is_some());
        assert!(supported_version(&body(2, 0)).is_some());
        assert!(supported_version(&body(0, 0)).is_none());
        assert!(supported_version(&body(3, 0)).is_none());
        assert!(supported_version(&[]).is_none());
    }

    /// Builds a request body with the given operation attributes, each as
    /// (value-tag, name).
    fn body_with(attrs: &[(u8, &str)]) -> Vec<u8> {
        let mut body = vec![2, 0, 0, 0x0B, 0, 0, 0, 1];
        body.push(DelimiterTag::OperationAttributes as u8);
        for (tag, name) in attrs {
            body.push(*tag);
            body.extend_from_slice(&(name.len() as u16).to_be_bytes());
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(&5u16.to_be_bytes());
            body.extend_from_slice(b"utf-8");
        }
        body.push(DelimiterTag::EndOfAttributes as u8);
        body
    }

    #[test]
    fn the_required_attribute_pair_must_come_first_and_in_order() {
        assert!(has_required_preamble(&body_with(&[
            (TAG_CHARSET, "attributes-charset"),
            (TAG_NATURAL_LANGUAGE, "attributes-natural-language"),
        ])));

        // Swapped.
        assert!(!has_required_preamble(&body_with(&[
            (TAG_NATURAL_LANGUAGE, "attributes-natural-language"),
            (TAG_CHARSET, "attributes-charset"),
        ])));
        // Only one of them.
        assert!(!has_required_preamble(&body_with(&[(
            TAG_CHARSET,
            "attributes-charset"
        )])));
        // Neither.
        assert!(!has_required_preamble(&body_with(&[])));
        // Something else first.
        assert!(!has_required_preamble(&body_with(&[
            (0x42, "job-name"),
            (TAG_CHARSET, "attributes-charset"),
            (TAG_NATURAL_LANGUAGE, "attributes-natural-language"),
        ])));
        // Truncated.
        assert!(!has_required_preamble(&[2, 0, 0, 0x0B, 0, 0, 0, 1]));
    }

    /// Where a test request comes from.
    fn request_context() -> RequestContext {
        RequestContext {
            origin: std::net::Ipv4Addr::new(10, 0, 0, 1).into(),
            host: "printer.local:631".into(),
        }
    }

    /// Builds a request for `operation`, with the operation attributes given
    /// as (name, value).
    fn request_for(
        operation: ipp::model::Operation,
        attributes: &[(IppAttributeName, IppValue)],
    ) -> Bytes {
        let uri: http::Uri = "ipp://printer.local:631/ipp/print".parse().unwrap();
        let mut request =
            IppRequestResponse::new(IppVersion::v2_0(), operation, Some(uri)).unwrap();
        for (name, value) in attributes {
            request.attributes_mut().add(
                DelimiterTag::OperationAttributes,
                IppAttribute::new(*name, value.clone()),
            );
        }
        request.to_bytes()
    }

    #[tokio::test]
    async fn a_handled_request_is_summarised_for_the_access_log() {
        let (printer, _) = printer_with(vec![]);
        let body = request_for(
            ipp::model::Operation::ValidateJob,
            &[
                (
                    IppAttributeName::JOB_NAME,
                    IppValue::new_name_without_language("Report.pdf").unwrap(),
                ),
                (
                    IppAttributeName::DOCUMENT_FORMAT,
                    IppValue::new_mime_media_type("application/pdf").unwrap(),
                ),
                (
                    IppAttributeName::REQUESTING_USER_NAME,
                    IppValue::new_name_without_language("alice").unwrap(),
                ),
            ],
        );

        let summary = printer.handle(body, &request_context()).await.summary;
        assert_eq!(summary.operation, Some(Operations::VALIDATE_JOB));
        assert_eq!(summary.operation_name().as_deref(), Some("Validate-Job"));
        assert_eq!(summary.status, Some(StatusCode::SuccessfulOk));
        assert_eq!(summary.status_name().as_deref(), Some("successful-ok"));
        assert_eq!(summary.request_id, Some(1));
        assert_eq!(summary.job_name.as_deref(), Some("Report.pdf"));
        assert_eq!(summary.document_format.as_deref(), Some("application/pdf"));
        assert_eq!(summary.user.as_deref(), Some("alice"));
        // Validate-Job creates nothing.
        assert!(summary.job_id.is_none());
    }

    #[tokio::test]
    async fn the_summary_carries_the_id_of_a_job_the_request_created() {
        let (printer, _) = printer_with(vec![]);
        let body = request_for(ipp::model::Operation::CreateJob, &[]);

        let summary = printer.handle(body, &request_context()).await.summary;
        assert_eq!(summary.status, Some(StatusCode::SuccessfulOk));
        let created = summary.job_id.expect("Create-Job answers with a job id");

        // And a request about that job is summarised with the id it names.
        let body = request_for(
            ipp::model::Operation::GetJobAttributes,
            &[(IppAttributeName::JOB_ID, IppValue::Integer(created))],
        );
        let summary = printer.handle(body, &request_context()).await.summary;
        assert_eq!(summary.job_id, Some(created));
        assert_eq!(
            summary.operation_name().as_deref(),
            Some("Get-Job-Attributes")
        );
    }

    #[tokio::test]
    async fn a_request_refused_before_it_parses_is_still_summarised() {
        let (printer, _) = printer_with(vec![]);
        // IPP 3.0, which this Printer does not speak.
        let body = Bytes::from_static(&[3, 0, 0, 0x0B, 0, 0, 0, 7]);

        let summary = printer.handle(body, &request_context()).await.summary;
        assert_eq!(
            summary.status,
            Some(StatusCode::ServerErrorVersionNotSupported)
        );
        assert_eq!(
            summary.status_name().as_deref(),
            Some("server-error-version-not-supported")
        );
        // The request-id is readable even when nothing else is.
        assert_eq!(summary.request_id, Some(7));
        assert!(summary.operation.is_none());
        assert!(summary.operation_name().is_none());
    }

    #[test]
    fn operation_and_status_names_are_spelled_the_way_ipp_spells_them() {
        assert_eq!(hyphenate("PrintJob"), "Print-Job");
        assert_eq!(hyphenate("GetPrinterAttributes"), "Get-Printer-Attributes");
        // An acronym stays in one piece.
        assert_eq!(hyphenate("CupsGetPPD"), "Cups-Get-PPD");
        assert_eq!(hyphenate("Ok"), "Ok");
        assert_eq!(hyphenate(""), "");

        // An operation this Printer does not implement still gets a name.
        let summary = IppSummary {
            operation: Some(0x000C),
            ..IppSummary::default()
        };
        assert_eq!(summary.operation_name().as_deref(), Some("Hold-Job"));
        // One that is not an IPP operation at all has none.
        let summary = IppSummary {
            operation: Some(0x7FFF),
            ..IppSummary::default()
        };
        assert!(summary.operation_name().is_none());
    }

    #[test]
    fn requested_attribute_filtering_honours_all_and_exact_names() {
        let requested = vec!["printer-name".to_string(), "printer-state".to_string()];
        assert!(wanted(Some(&requested), "printer-name"));
        assert!(!wanted(Some(&requested), "printer-info"));
        // No list at all means everything.
        assert!(wanted(None, "anything"));
        // 'all' means everything too.
        assert!(wanted(Some(&["all".to_string()]), "printer-info"));
    }

    #[test]
    fn names_are_stripped_of_control_characters_and_bounded() {
        assert_eq!(sanitise("job\u{0}name"), "jobname");
        assert_eq!(sanitise("  spaced  "), "spaced");
        assert_eq!(sanitise(""), "Untitled");
        assert_eq!(sanitise("\u{7}"), "Untitled");
        let long: String = "é".repeat(400);
        let cut = sanitise(&long);
        assert_eq!(cut.chars().count(), 255);
        assert!(cut.is_char_boundary(cut.len()));
    }

    #[test]
    fn gzip_and_both_flavours_of_deflate_round_trip() {
        use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
        use std::io::Write;

        let payload = b"%PDF-1.7 hello hello hello";

        let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(payload).unwrap();
        assert_eq!(decompress_gzip(&gz.finish().unwrap()).unwrap(), payload);

        let mut zlib = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        zlib.write_all(payload).unwrap();
        assert_eq!(
            decompress_deflate(&zlib.finish().unwrap()).unwrap(),
            payload
        );

        let mut raw = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        raw.write_all(payload).unwrap();
        assert_eq!(decompress_deflate(&raw.finish().unwrap()).unwrap(), payload);

        assert!(decompress_gzip(b"not compressed at all").is_err());
    }
}
