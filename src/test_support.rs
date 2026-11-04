//! Fixtures shared by this crate's tests.

use std::net::Ipv4Addr;

use uuid::Uuid;

use crate::attributes::PrinterContext;
use crate::job::{Clock, Job, Moment};
use crate::{PrinterConfig, PrinterState};

/// A configuration with every field set to something recognisable.
pub fn test_config() -> PrinterConfig {
    PrinterConfig {
        name: "Test Printer".into(),
        info: "A printer for tests".into(),
        location: "Hall 2".into(),
        manufacturer: "Test Co".into(),
        make_and_model: "Test Printer 1".into(),
        organization: "Acme Corp".into(),
        organizational_unit: "Printing".into(),
        uuid: Uuid::from_u128(0x0123_4567_89AB_CDEF_0123_4567_89AB_CDEF),
        ..PrinterConfig::default()
    }
}

/// A context describing a printer that has just started and is idle.
pub fn context(config: &PrinterConfig) -> PrinterContext<'_> {
    let clock = Clock::new();
    let now = Moment::now(&clock);
    PrinterContext {
        config,
        host: "printer.local:631",
        uptime: 42,
        queued: 0,
        state: PrinterState::Idle,
        state_change: now,
        config_change: now,
    }
}

/// A job that has been created but not yet printed.
pub fn pending_job() -> Job {
    let clock = Clock::new();
    Job {
        id: 1,
        uuid: Uuid::from_u128(1),
        name: "a job".into(),
        user: "tester".into(),
        origin: Ipv4Addr::new(10, 0, 0, 1).into(),
        state: crate::JobState::Pending,
        reasons: vec!["none".into()],
        message: String::new(),
        impressions: 3,
        impressions_completed: 0,
        k_octets: 12,
        document_format: "application/pdf".into(),
        created: Moment::now(&clock),
        processing: None,
        completed: None,
        template: Vec::new(),
        cancel_requested: false,
        has_document: true,
    }
}
