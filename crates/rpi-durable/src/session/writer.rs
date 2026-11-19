//! Group-commit Writer (port of `packages/durable/src/session/writer.ts`
//! @ `b4afdddf`). Storage runs off the mutation line; adoption and publication
//! run here in admission order without reacquiring that line.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};

use futures::FutureExt;
use tokio::sync::{oneshot, watch};

use crate::chord::context::{BACKGROUND_CONTEXT, Context, without_abort_signal};
use crate::errors::StorageError;
use crate::ids::Seq;
use crate::session::session::{SessionImpl, panic_message};
use crate::session::{SessionError, SessionResult};
use crate::types::{CommitResult, Storage, StorageWrite};

pub(crate) enum JobOutcome {
    Stored(Seq),
    Held,
    Discard,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishFailureKind {
    Adoption,
    Listener,
}

pub(crate) struct PublishFailure {
    pub(crate) kind: PublishFailureKind,
    pub(crate) error: SessionError,
}

impl PublishFailure {
    pub(crate) fn adoption(error: SessionError) -> Self {
        Self {
            kind: PublishFailureKind::Adoption,
            error,
        }
    }

    pub(crate) fn listener(error: SessionError) -> Self {
        Self {
            kind: PublishFailureKind::Listener,
            error,
        }
    }
}

type RunJob = Box<dyn FnOnce(JobOutcome) -> Result<(), PublishFailure> + Send>;

pub(crate) struct CommitJob {
    writes: Option<Vec<StorageWrite>>,
    keys: Vec<String>,
    context: Context,
    run: Option<RunJob>,
    completion: Option<oneshot::Sender<SessionResult<()>>>,
}

impl CommitJob {
    pub(crate) fn stored(
        writes: Vec<StorageWrite>,
        keys: Vec<String>,
        context: Context,
        run: RunJob,
    ) -> Self {
        Self::new(Some(writes), keys, context, run)
    }

    pub(crate) fn held(keys: Vec<String>, context: Context, run: RunJob) -> Self {
        Self::new(None, keys, context, run)
    }

    fn new(
        writes: Option<Vec<StorageWrite>>,
        keys: Vec<String>,
        context: Context,
        run: RunJob,
    ) -> Self {
        Self {
            writes,
            keys,
            context,
            run: Some(run),
            completion: None,
        }
    }
}

struct Gate {
    sender: watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
}

impl Gate {
    fn new() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self { sender, receiver }
    }
}

struct Part {
    job: CommitJob,
    gate: Arc<Gate>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Part>,
    flight: bool,
    shutdown: bool,
    unpublished: HashMap<String, Arc<Gate>>,
    newest: Option<Arc<Gate>>,
}

pub(crate) struct Writer {
    storage: Arc<dyn Storage>,
    session: Weak<SessionImpl>,
    state: Mutex<State>,
}

impl Writer {
    pub(crate) fn new(storage: Arc<dyn Storage>, session: Weak<SessionImpl>) -> Arc<Self> {
        Arc::new(Self {
            storage,
            session,
            state: Mutex::new(State::default()),
        })
    }

    pub(crate) fn admit(
        self: &Arc<Self>,
        mut job: CommitJob,
    ) -> oneshot::Receiver<SessionResult<()>> {
        let (completion, outcome) = oneshot::channel();
        job.completion = Some(completion);
        let gate = Arc::new(Gate::new());
        let start = {
            let Ok(mut state) = self.state.lock() else {
                drop(job);
                return outcome;
            };
            if state.shutdown {
                drop(state);
                let mut part = Part { job, gate };
                self.discard(&mut part, SessionError::Closed);
                return outcome;
            }
            for key in &job.keys {
                state.unpublished.insert(key.clone(), Arc::clone(&gate));
            }
            state.newest = Some(Arc::clone(&gate));
            state.queue.push_back(Part { job, gate });
            if state.flight {
                false
            } else {
                state.flight = true;
                true
            }
        };
        if start {
            let this = Arc::clone(self);
            tokio::spawn(async move { this.run().await });
        }
        outcome
    }

    fn gate(&self, key: &str) -> Option<Arc<Gate>> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.unpublished.get(key).cloned())
    }

    pub(crate) fn has_key(&self, key: &str) -> bool {
        self.state
            .lock()
            .ok()
            .is_some_and(|state| state.unpublished.contains_key(key))
    }

    fn drained_gate(&self) -> Option<Arc<Gate>> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.newest.clone())
    }

    pub(crate) async fn wait_key(&self, key: &str) {
        let gate = if key == crate::session::keys::ALL_KEYS {
            self.drained_gate()
        } else {
            self.gate(key)
        };
        if let Some(gate) = gate {
            wait_gate(gate).await;
        }
    }

    pub(crate) async fn drained(&self) {
        if let Some(gate) = self.drained_gate() {
            wait_gate(gate).await;
        }
    }

    pub(crate) fn shutdown(&self) {
        let queued = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.shutdown = true;
            state.queue.drain(..).collect::<Vec<_>>()
        };
        for mut part in queued {
            self.discard(&mut part, SessionError::Closed);
        }
    }

    async fn run(self: Arc<Self>) {
        loop {
            let mut parts = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                if state.queue.is_empty() {
                    state.flight = false;
                    return;
                }
                state.queue.drain(..).collect::<Vec<_>>()
            };
            let outcome = std::panic::AssertUnwindSafe(self.flush(&mut parts))
                .catch_unwind()
                .await;
            if let Err(payload) = outcome {
                let cause = panic_message(payload);
                if let Some(session) = self.session.upgrade() {
                    session.fail(cause.clone());
                }
                self.abandon(&mut parts, cause);
            }
        }
    }

    async fn flush(&self, parts: &mut [Part]) {
        if let Some(cause) = self.failure() {
            self.abandon(parts, cause);
            return;
        }
        let stored = parts
            .iter()
            .filter(|part| part.job.writes.is_some())
            .count();
        let mut seqs = vec![None; stored];
        let outcome = if stored == 0 {
            Ok(CommitResult {
                stored: 0,
                refused: None,
            })
        } else {
            let batches = parts
                .iter()
                .filter_map(|part| part.job.writes.clone())
                .collect::<Vec<_>>();
            let context = parts
                .iter()
                .find(|part| part.job.writes.is_some())
                .map(|part| without_abort_signal(&part.job.context))
                .unwrap_or_else(|| BACKGROUND_CONTEXT.clone());
            let mut receive = |index, seq| {
                if let Some(slot) = seqs.get_mut(index) {
                    *slot = Some(seq);
                }
            };
            self.storage.commit(&batches, &mut receive, &context).await
        };
        if let Some(cause) = self.failure() {
            self.abandon(parts, cause);
            return;
        }

        let (mut storage_error, mut result) = match outcome {
            Ok(result) => (None, Some(result)),
            Err(error) => (Some(error), None),
        };
        let storage_cause = storage_error.as_ref().map(ToString::to_string);
        let mut stop: Option<(String, bool)> = None;
        let mut stored_index = 0usize;
        let mut published = 0usize;
        for part in parts.iter_mut() {
            let stored = part.job.writes.is_some();
            let index = stored.then(|| {
                let index = stored_index;
                stored_index += 1;
                index
            });
            if let Some((cause, external)) = &stop {
                let error = if !external
                    && stored
                    && let Some(storage_cause) = &storage_cause
                {
                    take_storage_error(&mut storage_error, storage_cause)
                } else {
                    SessionError::Failed {
                        cause: self.failure().unwrap_or_else(|| {
                            storage_cause.clone().unwrap_or_else(|| cause.clone())
                        }),
                    }
                };
                self.discard(part, error);
                continue;
            }
            let seq = index.and_then(|index| seqs.get(index).copied().flatten());
            if stored && seq.is_none() {
                let error = if let Some(storage_cause) = &storage_cause {
                    take_storage_error(&mut storage_error, storage_cause)
                } else {
                    let refusal = result.as_mut().and_then(|result| {
                        (result.stored == index.expect("stored index"))
                            .then(|| result.refused.take())
                            .flatten()
                    });
                    match refusal {
                        Some(error) => SessionError::Storage(error),
                        None => SessionError::Storage(StorageError::message(
                            "Storage committed without adopting the batch",
                        )),
                    }
                };
                let cause = error.to_string();
                self.discard(part, error);
                stop = Some((cause, false));
                continue;
            }

            if published > 0 {
                tokio::task::yield_now().await;
            }
            if let Some(cause) = self.failure() {
                self.discard(
                    part,
                    SessionError::Failed {
                        cause: cause.clone(),
                    },
                );
                stop = Some((cause, true));
                continue;
            }
            let run = part.job.run.take().expect("commit job run");
            let run_result = match (stored, seq) {
                (true, Some(seq)) => run(JobOutcome::Stored(seq)),
                (false, _) => run(JobOutcome::Held),
                (true, None) => unreachable!(),
            };
            published += 1;
            match run_result {
                Ok(()) => {
                    if let Some(cause) = self.failure() {
                        self.complete(
                            part,
                            Err(SessionError::Failed {
                                cause: cause.clone(),
                            }),
                        );
                        stop = Some((cause, true));
                    } else if let Some(storage_cause) = &storage_cause {
                        let error = take_storage_error(&mut storage_error, storage_cause);
                        self.complete(part, Err(error));
                    } else {
                        self.complete(part, Ok(()));
                    }
                }
                Err(failure)
                    if failure.kind == PublishFailureKind::Listener
                        && let Some(storage_cause) = &storage_cause =>
                {
                    if let Some(session) = self.session.upgrade() {
                        session.report_listener_error(
                            failure.error.to_string(),
                            Some(part.job.context.clone()),
                        );
                    }
                    let error = take_storage_error(&mut storage_error, storage_cause);
                    self.complete(part, Err(error));
                }
                // A component listener throw is not the commit's failure: the
                // commit stands, the Session fails after it, and the parts
                // after it are not published (upstream writer.ts #publish).
                Err(failure) if failure.kind == PublishFailureKind::Listener => {
                    let cause = failure.error.to_string();
                    self.complete(part, Ok(()));
                    stop = Some((cause, false));
                }
                Err(failure) => {
                    let cause = failure.error.to_string();
                    self.complete(part, Err(failure.error));
                    stop = Some((cause, false));
                }
            }
        }
        let cause = storage_cause.or_else(|| stop.map(|(cause, _)| cause));
        if let Some(cause) = cause
            && let Some(session) = self.session.upgrade()
        {
            session.fail(cause);
        }
    }

    fn failure(&self) -> Option<String> {
        self.session.upgrade().and_then(|session| session.failure())
    }

    fn abandon(&self, parts: &mut [Part], cause: String) {
        for part in parts {
            self.discard(
                part,
                SessionError::Failed {
                    cause: cause.clone(),
                },
            );
        }
    }

    fn discard(&self, part: &mut Part, error: SessionError) {
        if let Some(run) = part.job.run.take() {
            let _ = run(JobOutcome::Discard);
        }
        if let Some(completion) = part.job.completion.take() {
            let _ = completion.send(Err(error));
        }
        self.open(part);
    }

    fn complete(&self, part: &mut Part, result: SessionResult<()>) {
        if let Some(completion) = part.job.completion.take() {
            let _ = completion.send(result);
        }
        self.open(part);
        if let Some(session) = self.session.upgrade() {
            session.schedule_eviction();
        }
    }

    fn open(&self, part: &Part) {
        if let Ok(mut state) = self.state.lock() {
            for key in &part.job.keys {
                if state
                    .unpublished
                    .get(key)
                    .is_some_and(|gate| Arc::ptr_eq(gate, &part.gate))
                {
                    state.unpublished.remove(key);
                }
            }
            if state
                .newest
                .as_ref()
                .is_some_and(|gate| Arc::ptr_eq(gate, &part.gate))
            {
                state.newest = None;
            }
        }
        let _ = part.gate.sender.send(true);
    }
}

fn take_storage_error(error: &mut Option<StorageError>, cause: &str) -> SessionError {
    SessionError::Storage(
        error
            .take()
            .unwrap_or_else(|| StorageError::message(cause.to_string())),
    )
}

async fn wait_gate(gate: Arc<Gate>) {
    let mut receiver = gate.receiver.clone();
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}
