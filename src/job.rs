//! Job objects and store.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, Utc};
use ipp::attribute::IppAttribute;
use ipp::model::JobState;
use uuid::Uuid;

/// Clients can ask for a job after it has been completed.
pub const DEFAULT_HISTORY: usize = 100;

/// The printer's clock.
#[derive(Debug)]
pub struct Clock {
    /// Used for monotonic time, for `printer-up-time` and `time-at-creation`.
    started: Instant,
    /// Used for wall-clock time.
    started_at: DateTime<Utc>,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            started: Instant::now(),
            started_at: Utc::now(),
        }
    }

    /// Seconds since the Printer started. `printer-up-time` must be > 0, so
    /// this never returns zero.
    pub fn ticks(&self) -> i32 {
        (self.started.elapsed().as_secs() as i32).saturating_add(1)
    }

    /// The current wall-clock time.
    pub fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    /// When the Printer started.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// A moment, recorded both ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    pub ticks: i32,
    pub at: DateTime<Utc>,
}

impl Moment {
    pub fn now(clock: &Clock) -> Self {
        Moment {
            ticks: clock.ticks(),
            at: clock.now(),
        }
    }
}

/// A print job.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: i32,
    pub uuid: Uuid,
    pub name: String,
    pub user: String,
    /// Where the submission came from, used for the per-client queue limit.
    pub origin: IpAddr,
    pub state: JobState,
    pub reasons: Vec<String>,
    pub message: String,
    /// Pages the job will produce; 0 until the document has been inspected.
    pub impressions: i32,
    pub impressions_completed: i32,
    /// Document size in kilo-octets, for `job-k-octets`.
    pub k_octets: i32,
    pub document_format: String,
    pub created: Moment,
    pub processing: Option<Moment>,
    pub completed: Option<Moment>,
    /// Job Template attributes as accepted, echoed back by
    /// Get-Job-Attributes.
    pub template: Vec<IppAttribute>,
    /// Set when the Client asked to cancel while the job was being printed.
    pub cancel_requested: bool,
    /// True once a document has been attached, so a second Send-Document can
    /// be refused.
    pub has_document: bool,
}

impl Job {
    /// Whether the job is still queued or running.
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            JobState::Pending
                | JobState::PendingHeld
                | JobState::Processing
                | JobState::ProcessingStopped
        )
    }

    /// Whether the job has reached a final state.
    pub fn is_finished(&self) -> bool {
        !self.is_active()
    }

    /// Whether the job matches a `which-jobs` keyword.
    pub fn matches(&self, which: &str) -> bool {
        match which {
            "completed" => matches!(
                self.state,
                JobState::Completed | JobState::Canceled | JobState::Aborted
            ),
            "not-completed" => self.is_active(),
            "pending" => self.state == JobState::Pending,
            "pending-held" => self.state == JobState::PendingHeld,
            "processing" => self.state == JobState::Processing,
            "processing-stopped" => self.state == JobState::ProcessingStopped,
            "canceled" => self.state == JobState::Canceled,
            "aborted" => self.state == JobState::Aborted,
            "all" => true,
            _ => false,
        }
    }

    /// Moves the job to a final state and records when.
    pub fn finish(&mut self, clock: &Clock, state: JobState, reason: &str, message: &str) {
        self.state = state;
        self.reasons = vec![reason.to_string()];
        self.message = message.to_string();
        self.completed = Some(Moment::now(clock));
        if state == JobState::Completed {
            self.impressions_completed = self.impressions;
        }
    }
}

/// The `which-jobs` keywords this Printer understands.
pub const WHICH_JOBS: &[&str] = &[
    "completed",
    "not-completed",
    "aborted",
    "all",
    "canceled",
    "pending",
    "pending-held",
    "processing",
    "processing-stopped",
];

/// Why a job could not be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// This client already has as many jobs queued as it is allowed.
    TooManyJobs { queued: usize, limit: usize },
}

/// Every job the Printer knows about.
#[derive(Debug)]
pub struct JobStore {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    next_id: i32,
    jobs: BTreeMap<i32, Job>,
    history: usize,
    /// How many jobs one address may have queued at once.
    per_origin: Option<usize>,
}

impl JobStore {
    pub fn new(history: usize, per_origin: Option<usize>) -> Self {
        JobStore {
            inner: Mutex::new(Inner {
                // Job ids start at 1; starting higher makes them look less
                // like array indices in logs, which has saved confusion
                // before, and IPP only requires them to be positive.
                next_id: 1,
                jobs: BTreeMap::new(),
                history,
                per_origin,
            }),
        }
    }

    /// Creates a job, unless this client already has too many queued.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        clock: &Clock,
        origin: IpAddr,
        user: String,
        name: String,
        document_format: String,
        template: Vec<IppAttribute>,
    ) -> Result<Job, CreateError> {
        let mut inner = self.inner.lock().expect("job store poisoned");

        if let Some(limit) = inner.per_origin {
            let queued = inner
                .jobs
                .values()
                .filter(|j| j.origin == origin && j.is_active())
                .count();
            if queued >= limit {
                return Err(CreateError::TooManyJobs { queued, limit });
            }
        }

        let id = inner.next_id;
        inner.next_id += 1;

        let job = Job {
            id,
            uuid: Uuid::new_v4(),
            name,
            user,
            origin,
            state: JobState::Pending,
            reasons: vec!["none".to_string()],
            message: String::new(),
            impressions: 0,
            impressions_completed: 0,
            k_octets: 0,
            document_format,
            created: Moment::now(clock),
            processing: None,
            completed: None,
            template,
            cancel_requested: false,
            has_document: false,
        };

        inner.jobs.insert(id, job.clone());
        Self::prune(&mut inner);
        Ok(job)
    }

    /// Drops the oldest finished jobs once there are more than `history`.
    fn prune(inner: &mut Inner) {
        let finished: Vec<i32> = inner
            .jobs
            .values()
            .filter(|j| j.is_finished())
            .map(|j| j.id)
            .collect();
        let excess = finished.len().saturating_sub(inner.history);
        // `jobs` is keyed by ascending id, so the first are the oldest.
        for id in finished.into_iter().take(excess) {
            inner.jobs.remove(&id);
        }
    }

    /// A snapshot of one job.
    pub fn get(&self, id: i32) -> Option<Job> {
        self.inner
            .lock()
            .expect("job store poisoned")
            .jobs
            .get(&id)
            .cloned()
    }

    /// Applies `change` to a job, returning the updated snapshot.
    pub fn update<F: FnOnce(&mut Job)>(&self, id: i32, change: F) -> Option<Job> {
        let mut inner = self.inner.lock().expect("job store poisoned");
        let job = inner.jobs.get_mut(&id)?;
        change(job);
        let snapshot = job.clone();
        Self::prune(&mut inner);
        Some(snapshot)
    }

    /// Jobs matching `which`, oldest first, at most `limit` of them.
    pub fn list(&self, which: &str, user: Option<&str>, limit: Option<usize>) -> Vec<Job> {
        let inner = self.inner.lock().expect("job store poisoned");
        inner
            .jobs
            .values()
            .filter(|job| job.matches(which))
            .filter(|job| user.is_none_or(|u| job.user == u))
            .take(limit.unwrap_or(usize::MAX))
            .cloned()
            .collect()
    }

    /// How many jobs are queued or running, for `queued-job-count`.
    pub fn active_count(&self) -> usize {
        self.inner
            .lock()
            .expect("job store poisoned")
            .jobs
            .values()
            .filter(|j| j.is_active())
            .count()
    }

    /// How many jobs are ahead of `id` in the queue.
    pub fn intervening(&self, id: i32) -> i32 {
        self.inner
            .lock()
            .expect("job store poisoned")
            .jobs
            .values()
            .filter(|j| j.is_active() && j.id < id)
            .count() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn origin(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    fn store() -> (JobStore, Clock) {
        (JobStore::new(DEFAULT_HISTORY, None), Clock::new())
    }

    fn create(store: &JobStore, clock: &Clock, from: u8) -> Result<Job, CreateError> {
        store.create(
            clock,
            origin(from),
            "tester".into(),
            "a job".into(),
            "application/pdf".into(),
            Vec::new(),
        )
    }

    #[test]
    fn jobs_start_pending_and_ids_only_go_up() {
        let (store, clock) = store();
        let first = create(&store, &clock, 1).unwrap();
        let second = create(&store, &clock, 1).unwrap();

        assert_eq!(first.state, JobState::Pending);
        assert!(second.id > first.id);
        assert_ne!(first.uuid, second.uuid);

        // Finishing and pruning must not let an id come back.
        store.update(first.id, |j| {
            j.finish(
                &clock,
                JobState::Completed,
                "job-completed-successfully",
                "",
            )
        });
        let third = create(&store, &clock, 1).unwrap();
        assert!(third.id > second.id);
    }

    #[test]
    fn the_queue_limit_counts_only_this_clients_active_jobs() {
        let clock = Clock::new();
        let store = JobStore::new(DEFAULT_HISTORY, Some(2));

        let first = create(&store, &clock, 1).unwrap();
        create(&store, &clock, 1).unwrap();
        // A third from the same address is refused...
        assert_eq!(
            create(&store, &clock, 1).err(),
            Some(CreateError::TooManyJobs {
                queued: 2,
                limit: 2
            })
        );
        // ...but another address is unaffected.
        assert!(create(&store, &clock, 2).is_ok());

        // Finishing one frees a slot.
        store.update(first.id, |j| {
            j.finish(
                &clock,
                JobState::Completed,
                "job-completed-successfully",
                "",
            )
        });
        assert!(create(&store, &clock, 1).is_ok());
    }

    #[test]
    fn which_jobs_filters_as_rfc_8011_describes() {
        let (store, clock) = store();
        let pending = create(&store, &clock, 1).unwrap();
        let done = create(&store, &clock, 1).unwrap();
        store.update(done.id, |j| {
            j.finish(
                &clock,
                JobState::Completed,
                "job-completed-successfully",
                "",
            )
        });

        let ids =
            |which| -> Vec<i32> { store.list(which, None, None).iter().map(|j| j.id).collect() };
        assert_eq!(ids("not-completed"), vec![pending.id]);
        assert_eq!(ids("completed"), vec![done.id]);
        assert_eq!(ids("all").len(), 2);
        assert_eq!(ids("pending"), vec![pending.id]);
        // A canceled job counts as completed for this purpose.
        store.update(pending.id, |j| {
            j.finish(&clock, JobState::Canceled, "job-canceled-by-user", "")
        });
        assert_eq!(ids("completed").len(), 2);
        assert!(ids("not-completed").is_empty());
        // An unknown keyword matches nothing rather than everything.
        assert!(ids("sideways").is_empty());
    }

    #[test]
    fn listing_can_be_limited_and_filtered_by_user() {
        let (store, clock) = store();
        for _ in 0..5 {
            create(&store, &clock, 1).unwrap();
        }
        store
            .create(
                &clock,
                origin(1),
                "someone-else".into(),
                "x".into(),
                "application/pdf".into(),
                Vec::new(),
            )
            .unwrap();

        assert_eq!(store.list("all", None, Some(3)).len(), 3);
        assert_eq!(store.list("all", Some("tester"), None).len(), 5);
        assert_eq!(store.list("all", Some("someone-else"), None).len(), 1);
        assert!(store.list("all", Some("nobody"), None).is_empty());
    }

    #[test]
    fn finished_jobs_are_kept_but_not_for_ever() {
        let clock = Clock::new();
        let store = JobStore::new(3, None);

        let mut ids = Vec::new();
        for _ in 0..6 {
            let job = create(&store, &clock, 1).unwrap();
            store.update(job.id, |j| {
                j.finish(
                    &clock,
                    JobState::Completed,
                    "job-completed-successfully",
                    "",
                )
            });
            ids.push(job.id);
        }

        // Only the three newest survive.
        assert_eq!(store.list("all", None, None).len(), 3);
        assert!(store.get(ids[0]).is_none());
        assert!(store.get(*ids.last().unwrap()).is_some());
    }

    #[test]
    fn active_jobs_are_never_pruned() {
        let clock = Clock::new();
        let store = JobStore::new(1, None);
        let active = create(&store, &clock, 1).unwrap();
        for _ in 0..5 {
            let job = create(&store, &clock, 1).unwrap();
            store.update(job.id, |j| {
                j.finish(
                    &clock,
                    JobState::Completed,
                    "job-completed-successfully",
                    "",
                )
            });
        }
        assert!(store.get(active.id).is_some());
        assert_eq!(store.active_count(), 1);
    }

    #[test]
    fn counting_reflects_the_queue() {
        let (store, clock) = store();
        let first = create(&store, &clock, 1).unwrap();
        let second = create(&store, &clock, 1).unwrap();
        assert_eq!(store.active_count(), 2);
        assert_eq!(store.intervening(second.id), 1);
        assert_eq!(store.intervening(first.id), 0);

        store.update(first.id, |j| {
            j.finish(
                &clock,
                JobState::Completed,
                "job-completed-successfully",
                "",
            )
        });
        assert_eq!(store.active_count(), 1);
        assert_eq!(store.intervening(second.id), 0);
    }

    #[test]
    fn completing_a_job_records_its_impressions_and_time() {
        let (store, clock) = store();
        let job = create(&store, &clock, 1).unwrap();
        store.update(job.id, |j| j.impressions = 7);
        let done = store
            .update(job.id, |j| {
                j.finish(
                    &clock,
                    JobState::Completed,
                    "job-completed-successfully",
                    "",
                )
            })
            .unwrap();

        assert_eq!(done.impressions_completed, 7);
        assert!(done.completed.is_some());
        assert!(done.is_finished());
        // A canceled job does not claim to have printed anything.
        let other = create(&store, &clock, 1).unwrap();
        store.update(other.id, |j| j.impressions = 7);
        let canceled = store
            .update(other.id, |j| {
                j.finish(&clock, JobState::Canceled, "job-canceled-by-user", "")
            })
            .unwrap();
        assert_eq!(canceled.impressions_completed, 0);
    }

    #[test]
    fn the_clock_never_reports_a_zero_uptime() {
        let clock = Clock::new();
        assert!(clock.ticks() > 0);
        assert!(clock.now() >= clock.started_at());
    }
}
