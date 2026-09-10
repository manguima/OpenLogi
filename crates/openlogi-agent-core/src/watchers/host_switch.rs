//! Keep configured keyboard → pointing-device host-switch links armed.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use openlogi_hid::{
    ChannelPool, ChannelRegistry, DeviceIoGate, DeviceRoute, HostSwitchRestoreOutcome,
    HostSwitchStopReason, PendingHostSwitchRestore, run_host_switch_session, switch_linked_hosts,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, warn};

use super::shutdown::{ManagerCompletion, WatcherHandle};
use crate::receiver_access::{ExclusiveAccessReason, ReceiverAccess, ReceiverRequestState};

const DEPARTURE_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(1);
/// Dead time after one externally requested move fails.
const REQUEST_BACKOFF_BASE: Duration = Duration::from_secs(2);
/// Ceiling for the doubling below, so a permanently deaf host still gets
/// retried when the user asks again minutes later.
const REQUEST_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Doublings applied to [`REQUEST_BACKOFF_BASE`] before the ceiling bites.
const REQUEST_BACKOFF_DOUBLINGS: u32 = 5;

/// One resolved link. Config keys are converted to live routes by the
/// orchestrator so the transport watcher never needs to understand inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSwitchLink {
    /// Keyboard whose host switch keys initiate the transition.
    pub keyboard: DeviceRoute,
    /// Pointing devices that follow the keyboard.
    pub targets: Vec<DeviceRoute>,
}

/// Read-only, lossless, coalescing view of resolved links.
pub type HostSwitchLinks = watch::Receiver<std::sync::Arc<Vec<HostSwitchLink>>>;

/// One ask, tagged so that repeating the same host still reads as new work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HostRequest {
    /// Monotonic id. Also what a caller waits to see settled.
    serial: u64,
    /// 1-based Easy-Switch channel.
    host: u8,
}

/// Asks the manager to move every configured link to a host.
///
/// The manager stays the single transition authority: a caller that switched
/// hosts on its own would race the capture sessions this module owns.
///
/// Asks coalesce into one slot rather than queueing. A host move is idempotent
/// and only-latest-matters, so a backlog of stale asks is not work to catch up
/// on — it is a caller outrunning the hardware, and every entry in it costs
/// another exclusive receiver lease. The single slot plus [`Self::request`]
/// waiting for settlement caps a caller at one ask in flight whatever rate it
/// polls at.
#[derive(Clone)]
pub struct HostSwitchRequester {
    asks: Arc<watch::Sender<Option<HostRequest>>>,
    settled: watch::Receiver<u64>,
}

impl std::fmt::Debug for HostSwitchRequester {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostSwitchRequester")
            .finish_non_exhaustive()
    }
}

impl HostSwitchRequester {
    /// Request a move to the 1-based Easy-Switch channel `host`, resolving once
    /// the manager has finished acting on it.
    ///
    /// Settled means carried out, superseded by a later ask, throttled after
    /// repeated failures, or abandoned because the manager shut down. A caller
    /// therefore measures its own dead time from the end of the transition
    /// rather than from the ask, which is the only way a poll loop can be
    /// structurally unable to queue a second move behind the first.
    pub async fn request(&self, host: u8) {
        let mut settled = self.settled.clone();
        let mut serial = 0;
        // Assigning the serial inside the send keeps the id and the slot in one
        // critical section, so concurrent callers cannot publish out of order.
        self.asks.send_modify(|slot| {
            serial = slot.map_or(1, |previous| previous.serial.saturating_add(1));
            *slot = Some(HostRequest { serial, host });
        });
        while *settled.borrow_and_update() < serial {
            if settled.changed().await.is_err() {
                return;
            }
        }
    }
}

struct HostSwitchManagerContext {
    links: HostSwitchLinks,
    channel_pool: ChannelPool,
    registry: ChannelRegistry,
    receiver_access: ReceiverAccess,
    receiver_requests: watch::Receiver<ReceiverRequestState>,
    device_io: DeviceIoGate,
    requests: watch::Receiver<Option<HostRequest>>,
    settlements: watch::Sender<u64>,
    shutdown: oneshot::Receiver<()>,
}

/// Spawn the host switch session manager.
///
/// The returned requester is how anything other than an Easy-Switch key press
/// asks for a transition; dropping it leaves key presses as the only trigger.
#[must_use]
pub fn spawn(
    links: &HostSwitchLinks,
    channel_pool: ChannelPool,
    receiver_access: ReceiverAccess,
    registry: ChannelRegistry,
    device_io: DeviceIoGate,
) -> (WatcherHandle, HostSwitchRequester) {
    let links = links.clone();
    let receiver_requests = receiver_access.subscribe_requests();
    let (requests_tx, requests_rx) = watch::channel(None);
    let (settlements_tx, settlements_rx) = watch::channel(0);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (shutdown_done_tx, shutdown_done_rx) = oneshot::channel();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                warn!(%error, "host switch watcher: could not build tokio runtime");
                let _ = shutdown_done_tx.send(ManagerCompletion::Unexpected);
                return;
            }
        };
        let completion = runtime.block_on(manage(HostSwitchManagerContext {
            links,
            channel_pool,
            registry,
            receiver_access,
            receiver_requests,
            device_io,
            requests: requests_rx,
            settlements: settlements_tx,
            shutdown: shutdown_rx,
        }));
        // A manager return can strand detached task supervisors. Destroy their
        // runtime before reporting that no old firmware writer remains.
        drop(runtime);
        let _ = shutdown_done_tx.send(completion);
    });
    (
        WatcherHandle::new(shutdown_tx, shutdown_done_rx),
        HostSwitchRequester {
            asks: Arc::new(requests_tx),
            settled: settlements_rx,
        },
    )
}

enum SessionPhase {
    Active(oneshot::Sender<HostSwitchStopReason>),
    Draining,
}

struct RunningSession {
    link: HostSwitchLink,
    generation: u64,
    phase: SessionPhase,
}

impl RunningSession {
    fn stop(&mut self, reason: HostSwitchStopReason) {
        let SessionPhase::Active(stop) = std::mem::replace(&mut self.phase, SessionPhase::Draining)
        else {
            return;
        };
        let _ = stop.send(reason);
    }
}

enum RestorePhase {
    Ready {
        token: PendingHostSwitchRestore,
        retry_at: Instant,
    },
    Restoring,
}

struct Recovery {
    link: HostSwitchLink,
    generation: u64,
    requested_host: Option<u8>,
    restore: RestorePhase,
}

enum HostSwitchSlot {
    Running(RunningSession),
    Recovering(Recovery),
    Restarting {
        link: HostSwitchLink,
        retry_at: Instant,
    },
}

impl HostSwitchSlot {
    fn keyboard(&self) -> &DeviceRoute {
        match self {
            Self::Running(session) => &session.link.keyboard,
            Self::Recovering(recovery) => &recovery.link.keyboard,
            Self::Restarting { link, .. } => &link.keyboard,
        }
    }
}

/// What asked for a transition.
///
/// A request carries the ask's serial so a completion cannot be charged to the
/// ask that superseded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransitionSource {
    /// An Easy-Switch key press observed by a capture session.
    KeyPress,
    /// An external ask through [`HostSwitchRequester`].
    Request {
        /// Serial of the ask this transition belongs to.
        serial: u64,
    },
}

/// What one transition attempt achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransitionOutcome {
    /// The keyboard left this host, or there was nothing to do.
    Settled,
    /// The write was rejected, or the keyboard never left within the budget.
    Failed,
}

#[derive(Clone)]
struct TransitionIntent {
    link: HostSwitchLink,
    host: u8,
    source: TransitionSource,
}

enum TransitionPhase {
    Waiting(TransitionIntent),
    Running(TransitionSource),
}

struct SessionCompletion {
    generation: u64,
    result: Result<SessionResult, tokio::task::JoinError>,
}

struct SessionResult {
    requested_host: Option<u8>,
    pending_restore: Option<PendingHostSwitchRestore>,
    failed: bool,
}

struct RestoreCompletion {
    generation: u64,
    result: Result<HostSwitchRestoreOutcome, tokio::task::JoinError>,
}

enum ManagerEvent {
    Session(SessionCompletion),
    Restore(RestoreCompletion),
    Transition(Result<TransitionOutcome, tokio::task::JoinError>),
}

struct SessionServices {
    channel_pool: ChannelPool,
    registry: ChannelRegistry,
    receiver_access: ReceiverAccess,
    device_io: DeviceIoGate,
    events: mpsc::UnboundedSender<ManagerEvent>,
}

/// Consecutive-failure throttle for externally requested host moves.
///
/// A host that will not accept the switch must not cost a fresh exclusive
/// receiver lease — and up to [`DEPARTURE_TIMEOUT`] of starved HID++ sessions —
/// on every ask. Each consecutive failure doubles the dead time before another
/// attempt is allowed; a success, or an ask for a different host, clears it.
#[derive(Debug, Default)]
struct RequestBackoff {
    host: Option<u8>,
    failures: u32,
    blocked_until: Option<Instant>,
}

impl RequestBackoff {
    fn blocks(&self, host: u8, now: Instant) -> bool {
        self.host == Some(host) && self.blocked_until.is_some_and(|until| now < until)
    }

    fn record(&mut self, host: u8, failed: bool, now: Instant) {
        if self.host != Some(host) {
            self.host = Some(host);
            self.failures = 0;
        }
        if !failed {
            self.failures = 0;
            self.blocked_until = None;
            return;
        }
        self.failures = self.failures.saturating_add(1);
        let doublings = self
            .failures
            .saturating_sub(1)
            .min(REQUEST_BACKOFF_DOUBLINGS);
        let delay = REQUEST_BACKOFF_BASE
            .saturating_mul(2u32.saturating_pow(doublings))
            .min(REQUEST_BACKOFF_MAX);
        self.blocked_until = Some(now + delay);
    }
}

/// An outstanding request to move every link to one host.
///
/// Links move one at a time through the same transition slot an Easy-Switch
/// press uses, so `remaining` is what is left of one ask, not a queue of asks.
struct PendingRequest {
    serial: u64,
    host: u8,
    remaining: Vec<HostSwitchLink>,
    failed: bool,
}

struct HostSwitchManagerState {
    slots: Vec<HostSwitchSlot>,
    next_generation: u64,
    transition: Option<TransitionPhase>,
    request: Option<PendingRequest>,
    backoff: RequestBackoff,
    /// Highest ask serial already taken on, so re-reads are idempotent.
    seen: u64,
    /// Highest ask serial the manager is finished with.
    settled: u64,
    task_failed: bool,
}

impl HostSwitchManagerState {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            next_generation: 0,
            transition: None,
            request: None,
            backoff: RequestBackoff::default(),
            seen: 0,
            settled: 0,
            task_failed: false,
        }
    }

    /// Take on an externally requested move, superseding any earlier one.
    ///
    /// Re-reading the published ask is a no-op: only a serial past the highest
    /// one already taken on is new work.
    fn accept_request(&mut self, ask: HostRequest, published: &[HostSwitchLink], now: Instant) {
        if ask.serial <= self.seen {
            return;
        }
        self.seen = ask.serial;
        // Only the latest ask matters. Retiring the older one releases its
        // caller; it says nothing about the host, so it does not charge the
        // throttle. A transition already running for it keeps its own serial
        // and its outcome is discarded when it lands.
        if let Some(superseded) = self.request.take() {
            self.settled = self.settled.max(superseded.serial);
        }
        if matches!(
            &self.transition,
            Some(TransitionPhase::Waiting(intent))
                if matches!(intent.source, TransitionSource::Request { .. })
        ) {
            self.transition = None;
        }
        if self.backoff.blocks(ask.host, now) {
            debug!(
                host = ask.host,
                "host switch request throttled after repeated failures"
            );
            self.settled = self.settled.max(ask.serial);
            return;
        }
        self.request = Some(PendingRequest {
            serial: ask.serial,
            host: ask.host,
            remaining: published.to_vec(),
            failed: false,
        });
    }

    /// Hand the outstanding request's next still-published link to the
    /// transition slot, so it travels the same path a key press does, and
    /// retire the request once nothing is left to move.
    fn promote_request(&mut self, published: &[HostSwitchLink], terminal: bool, now: Instant) {
        if terminal {
            if let Some(abandoned) = self.request.take() {
                self.settled = self.settled.max(abandoned.serial);
            }
            return;
        }
        if self.transition.is_some() {
            return;
        }
        let Some(request) = self.request.as_mut() else {
            return;
        };
        while let Some(link) = request.remaining.pop() {
            if published.contains(&link) {
                self.transition = Some(TransitionPhase::Waiting(TransitionIntent {
                    link,
                    host: request.host,
                    source: TransitionSource::Request {
                        serial: request.serial,
                    },
                }));
                return;
            }
        }
        self.finish_request(now);
    }

    /// Charge the throttle for the finished ask and release its caller.
    fn finish_request(&mut self, now: Instant) {
        let Some(request) = self.request.take() else {
            return;
        };
        self.backoff.record(request.host, request.failed, now);
        self.settled = self.settled.max(request.serial);
    }

    /// Fold a landed transition into the ask that started it.
    ///
    /// A completion for a superseded ask is dropped: the successor owns the
    /// slot and must not inherit its predecessor's verdict.
    fn record_transition_outcome(&mut self, source: TransitionSource, outcome: TransitionOutcome) {
        let TransitionSource::Request { serial } = source else {
            return;
        };
        let Some(request) = self
            .request
            .as_mut()
            .filter(|request| request.serial == serial)
        else {
            return;
        };
        request.failed |= outcome == TransitionOutcome::Failed;
    }

    fn has_pending_restores(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot, HostSwitchSlot::Recovering(_)))
    }

    fn has_running_sessions(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot, HostSwitchSlot::Running(_)))
    }

    fn owns_keyboard(&self, keyboard: &DeviceRoute) -> bool {
        self.slots.iter().any(|slot| slot.keyboard() == keyboard)
    }

    fn reconcile_transition(&mut self, published: &[HostSwitchLink], terminal: bool) {
        if matches!(
            &self.transition,
            Some(TransitionPhase::Waiting(intent)) if terminal || !published.contains(&intent.link)
        ) {
            self.transition = None;
        }
    }

    fn begin_transition(&mut self, terminal: bool) -> Option<TransitionIntent> {
        if terminal || self.has_running_sessions() || self.has_pending_restores() {
            return None;
        }
        let Some(TransitionPhase::Waiting(intent)) = self
            .transition
            .take_if(|phase| matches!(phase, TransitionPhase::Waiting(_)))
        else {
            return None;
        };
        self.transition = Some(TransitionPhase::Running(intent.source));
        Some(intent)
    }

    fn terminal_completion(&self, terminal: bool) -> Option<ManagerCompletion> {
        (terminal
            && !self.has_running_sessions()
            && !self.has_pending_restores()
            && self.transition.is_none())
        .then_some(if self.task_failed {
            ManagerCompletion::Unexpected
        } else {
            ManagerCompletion::Graceful
        })
    }

    fn deadline(&self, requests: ReceiverRequestState, device_io_allowed: bool) -> Option<Instant> {
        if requests.any() || !device_io_allowed {
            return None;
        }
        self.slots
            .iter()
            .filter_map(|slot| match slot {
                HostSwitchSlot::Recovering(Recovery {
                    restore: RestorePhase::Ready { retry_at, .. },
                    ..
                })
                | HostSwitchSlot::Restarting { retry_at, .. } => Some(*retry_at),
                HostSwitchSlot::Running(_) | HostSwitchSlot::Recovering(_) => None,
            })
            .min()
    }

    fn stop_sessions(&mut self, wanted: &[HostSwitchLink], terminal: bool) {
        for slot in &mut self.slots {
            let HostSwitchSlot::Running(session) = slot else {
                continue;
            };
            if terminal || !wanted.contains(&session.link) {
                session.stop(HostSwitchStopReason::Graceful);
            }
        }
    }

    fn reconcile_recoveries(
        &mut self,
        published: &[HostSwitchLink],
        requests: ReceiverRequestState,
        services: &SessionServices,
        terminal: bool,
    ) {
        let now = Instant::now();
        self.slots.retain(|slot| match slot {
            HostSwitchSlot::Restarting { link, retry_at } => {
                !terminal && published.contains(link) && (*retry_at > now || requests.any())
            }
            HostSwitchSlot::Running(_) | HostSwitchSlot::Recovering(_) => true,
        });
        for slot in &mut self.slots {
            let HostSwitchSlot::Recovering(recovery) = slot else {
                continue;
            };
            if !published.contains(&recovery.link) {
                recovery.requested_host = None;
            }
            let RestorePhase::Ready { retry_at, .. } = &recovery.restore else {
                continue;
            };
            if *retry_at > now || requests.any() {
                continue;
            }
            let Some(lease) = services.receiver_access.try_acquire_for_session() else {
                break;
            };
            let generation = recovery.generation;
            let RestorePhase::Ready { token, .. } =
                std::mem::replace(&mut recovery.restore, RestorePhase::Restoring)
            else {
                continue;
            };
            let registry = services.registry.clone();
            let device_io = services.device_io.clone();
            let events = services.events.clone();
            tokio::spawn(async move {
                let task = tokio::spawn(async move {
                    let _lease = lease;
                    if device_io.allows_io() {
                        token.retry(&registry).await
                    } else {
                        HostSwitchRestoreOutcome::RestorePending(token)
                    }
                });
                let _ = events.send(ManagerEvent::Restore(RestoreCompletion {
                    generation,
                    result: task.await,
                }));
            });
        }
    }

    fn spawn_successors(&mut self, wanted: &[HostSwitchLink], services: &SessionServices) {
        for link in wanted {
            if self.owns_keyboard(&link.keyboard) {
                continue;
            }
            let Some(lease) = services.receiver_access.try_acquire_for_session() else {
                break;
            };
            self.next_generation = self.next_generation.wrapping_add(1);
            self.slots.push(HostSwitchSlot::Running(spawn_session(
                link.clone(),
                self.next_generation,
                lease,
                services,
            )));
        }
    }

    fn handle_session_completion(
        &mut self,
        completion: SessionCompletion,
        published: &[HostSwitchLink],
        terminal: bool,
    ) {
        let Some(index) = self.slots.iter().position(|slot| {
            matches!(slot, HostSwitchSlot::Running(session) if session.generation == completion.generation)
        }) else {
            return;
        };
        let HostSwitchSlot::Running(session) = self.slots.remove(index) else {
            return;
        };
        let result = match completion.result {
            Ok(result) => result,
            Err(error) => {
                warn!(%error, route = %session.link.keyboard, "host switch session task failed");
                self.task_failed = true;
                return;
            }
        };
        if result.failed {
            debug!(route = %session.link.keyboard, "host switch session ended");
        }
        let request_is_current = !terminal && published.contains(&session.link);
        if let Some(token) = result.pending_restore {
            self.slots.push(HostSwitchSlot::Recovering(Recovery {
                link: session.link,
                generation: session.generation,
                requested_host: result.requested_host.filter(|_| request_is_current),
                restore: RestorePhase::Ready {
                    token,
                    retry_at: Instant::now() + RETRY_DELAY,
                },
            }));
        } else if let Some(host) = result.requested_host.filter(|_| request_is_current) {
            self.transition = Some(TransitionPhase::Waiting(TransitionIntent {
                link: session.link,
                host,
                source: TransitionSource::KeyPress,
            }));
        } else if result.failed && request_is_current {
            self.slots.push(HostSwitchSlot::Restarting {
                link: session.link,
                retry_at: Instant::now() + RETRY_DELAY,
            });
        }
    }

    fn handle_restore_completion(
        &mut self,
        completion: RestoreCompletion,
        published: &[HostSwitchLink],
        terminal: bool,
    ) {
        let Some(index) = self.slots.iter().position(|slot| {
            matches!(slot, HostSwitchSlot::Recovering(recovery) if recovery.generation == completion.generation)
        }) else {
            return;
        };
        let HostSwitchSlot::Recovering(mut recovery) = self.slots.remove(index) else {
            return;
        };
        match completion.result {
            Ok(HostSwitchRestoreOutcome::RestorePending(token)) => {
                recovery.restore = RestorePhase::Ready {
                    token,
                    retry_at: Instant::now() + RETRY_DELAY,
                };
                self.slots.push(HostSwitchSlot::Recovering(recovery));
            }
            Ok(HostSwitchRestoreOutcome::Restored) => {
                let request_is_current = !terminal && published.contains(&recovery.link);
                if let Some(host) = recovery.requested_host.filter(|_| request_is_current) {
                    self.transition = Some(TransitionPhase::Waiting(TransitionIntent {
                        link: recovery.link,
                        host,
                        source: TransitionSource::KeyPress,
                    }));
                }
            }
            Err(error) => {
                warn!(%error, route = %recovery.link.keyboard, "host switch restore task failed");
                self.task_failed = true;
            }
        }
    }
}

async fn manage(context: HostSwitchManagerContext) -> ManagerCompletion {
    let HostSwitchManagerContext {
        mut links,
        channel_pool,
        registry,
        receiver_access,
        mut receiver_requests,
        mut device_io,
        requests: mut external_requests,
        settlements,
        mut shutdown,
    } = context;
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let mut registry_changes = registry.subscribe();
    let services = SessionServices {
        channel_pool,
        registry,
        receiver_access,
        device_io: device_io.clone(),
        events,
    };
    let mut state = HostSwitchManagerState::new();
    let mut terminal = false;
    let mut requests_open = true;

    loop {
        let requests = *receiver_requests.borrow_and_update();
        let published = std::sync::Arc::clone(&links.borrow_and_update());
        let io_allowed = device_io.allows_io();
        let now = Instant::now();
        if let Some(ask) = *external_requests.borrow_and_update() {
            state.accept_request(ask, &published, now);
        }
        state.reconcile_transition(&published, terminal);
        state.promote_request(&published, terminal, now);
        let _ = settlements.send_if_modified(|reported| {
            let advanced = *reported < state.settled;
            if advanced {
                *reported = state.settled;
            }
            advanced
        });
        let wanted = if terminal || requests.any() || state.transition.is_some() {
            &[][..]
        } else {
            published.as_slice()
        };
        if io_allowed || terminal {
            state.stop_sessions(wanted, terminal);
        }
        if io_allowed {
            state.reconcile_recoveries(&published, requests, &services, terminal);
            if !terminal && state.transition.is_none() {
                state.spawn_successors(wanted, &services);
            }
        }
        if let Some(completion) = state.terminal_completion(terminal) {
            return completion;
        }
        maybe_spawn_transition(&mut state, &links, &services, terminal);

        let deadline = state.deadline(*receiver_requests.borrow(), device_io.allows_io());
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            continue;
        }

        tokio::select! {
            biased;

            _ = &mut shutdown, if !terminal => {
                terminal = true;
            }
            Some(event) = event_rx.recv() => {
                let published = links.borrow().clone();
                handle_manager_event(&mut state, event, &published, terminal);
            }
            result = external_requests.changed(), if requests_open => {
                if result.is_err() {
                    // Every requester is gone, so Easy-Switch key presses are
                    // the only trigger left. Disable the arm: a closed watch
                    // resolves instantly and would otherwise spin the loop.
                    requests_open = false;
                }
            }
            result = links.changed() => {
                if result.is_err() {
                    return ManagerCompletion::Unexpected;
                }
            }
            result = receiver_requests.changed() => {
                if result.is_err() {
                    return ManagerCompletion::Unexpected;
                }
            }
            allowed = device_io.changed() => match allowed {
                Some(_) => {}
                None => return ManagerCompletion::Unexpected,
            },
            changed = registry_changes.changed() => {
                if changed.is_err() {
                    return ManagerCompletion::Unexpected;
                }
                expedite_pending_restores(&mut state);
            }
            () = wait_for_deadline(deadline) => {}
        }
    }
}

fn handle_manager_event(
    state: &mut HostSwitchManagerState,
    event: ManagerEvent,
    published: &[HostSwitchLink],
    terminal: bool,
) {
    match event {
        ManagerEvent::Session(completion) => {
            state.handle_session_completion(completion, published, terminal);
        }
        ManagerEvent::Restore(completion) => {
            state.handle_restore_completion(completion, published, terminal);
        }
        ManagerEvent::Transition(result) => {
            let source = match state.transition.take() {
                Some(TransitionPhase::Running(source)) => source,
                // Only `begin_transition` spawns a transition, and it always
                // installs `Running`; anything else means the slot no longer
                // describes this completion, so leave it as it stands.
                other => {
                    state.transition = other;
                    return;
                }
            };
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(error) => {
                    warn!(%error, "host transition task failed");
                    state.task_failed = true;
                    TransitionOutcome::Failed
                }
            };
            state.record_transition_outcome(source, outcome);
        }
    }
}

fn spawn_session(
    link: HostSwitchLink,
    generation: u64,
    receiver_lease: crate::receiver_access::SessionReceiverLease,
    services: &SessionServices,
) -> RunningSession {
    let (stop, stop_rx) = oneshot::channel();
    let session_link = link.clone();
    let registry = services.registry.clone();
    let device_io = services.device_io.clone();
    let events = services.events.clone();
    tokio::spawn(async move {
        let task = tokio::spawn(async move {
            let _receiver_lease = receiver_lease;
            match run_host_switch_session(
                session_link.keyboard.clone(),
                stop_rx,
                &registry,
                device_io,
            )
            .await
            {
                Ok(outcome) => {
                    let (requested_host, pending_restore) = outcome.into_parts();
                    SessionResult {
                        requested_host,
                        pending_restore,
                        failed: false,
                    }
                }
                Err(failure) => {
                    let (error, pending_restore) = failure.into_parts();
                    debug!(%error, route = %session_link.keyboard, "host switch session ended");
                    SessionResult {
                        requested_host: None,
                        pending_restore,
                        failed: true,
                    }
                }
            }
        });
        let _ = events.send(ManagerEvent::Session(SessionCompletion {
            generation,
            result: task.await,
        }));
    });
    RunningSession {
        link,
        generation,
        phase: SessionPhase::Active(stop),
    }
}

fn maybe_spawn_transition(
    state: &mut HostSwitchManagerState,
    links: &HostSwitchLinks,
    services: &SessionServices,
    terminal: bool,
) {
    let Some(intent) = state.begin_transition(terminal) else {
        return;
    };
    let links = links.clone();
    let pool = services.channel_pool.clone();
    let receiver_access = services.receiver_access.clone();
    let device_io = services.device_io.clone();
    let events = services.events.clone();
    tokio::spawn(async move {
        let task = tokio::spawn(run_transition(
            links,
            pool,
            receiver_access,
            device_io,
            intent,
        ));
        let _ = events.send(ManagerEvent::Transition(task.await));
    });
}

async fn run_transition(
    mut links: HostSwitchLinks,
    channel_pool: ChannelPool,
    receiver_access: ReceiverAccess,
    device_io: DeviceIoGate,
    intent: TransitionIntent,
) -> TransitionOutcome {
    let _lease = receiver_access
        .acquire_exclusive(ExclusiveAccessReason::HostTransition)
        .await;
    if !device_io.allows_io() || !links.borrow().contains(&intent.link) {
        return TransitionOutcome::Settled;
    }
    match switch_linked_hosts(
        &intent.link.keyboard,
        &intent.link.targets,
        intent.host,
        &channel_pool,
    )
    .await
    {
        Ok(true) => {
            if wait_for_departure(&mut links, &intent.link.keyboard).await {
                TransitionOutcome::Settled
            } else {
                TransitionOutcome::Failed
            }
        }
        // Already on that host: nothing moved, and nothing is wrong.
        Ok(false) => TransitionOutcome::Settled,
        Err(error) => {
            debug!(%error, route = %intent.link.keyboard, host = intent.host, "keyboard host switch failed");
            TransitionOutcome::Failed
        }
    }
}

fn expedite_pending_restores(state: &mut HostSwitchManagerState) {
    let now = Instant::now();
    for slot in &mut state.slots {
        if let HostSwitchSlot::Recovering(Recovery {
            restore: RestorePhase::Ready { retry_at, .. },
            ..
        }) = slot
        {
            *retry_at = now;
        }
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Whether the keyboard left this host within [`DEPARTURE_TIMEOUT`].
async fn wait_for_departure(links: &mut HostSwitchLinks, keyboard: &DeviceRoute) -> bool {
    let deadline = tokio::time::sleep(DEPARTURE_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        let departed = !links
            .borrow_and_update()
            .iter()
            .any(|link| link.keyboard == *keyboard);
        if departed {
            return true;
        }
        tokio::select! {
            result = links.changed() => {
                if result.is_err() {
                    return false;
                }
            }
            () = &mut deadline => {
                warn!(route = %keyboard, "host transition departure was not observed");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(slot: u8) -> DeviceRoute {
        DeviceRoute::Bolt {
            receiver_uid: "cafe".to_owned(),
            slot,
        }
    }

    fn link(target: u8) -> HostSwitchLink {
        HostSwitchLink {
            keyboard: route(1),
            targets: vec![route(target)],
        }
    }

    #[test]
    fn target_change_drains_old_session_before_successor_can_arm() {
        let (stop, mut stop_rx) = oneshot::channel();
        let mut state = HostSwitchManagerState::new();
        state.slots.push(HostSwitchSlot::Running(RunningSession {
            link: link(2),
            generation: 1,
            phase: SessionPhase::Active(stop),
        }));

        state.stop_sessions(&[link(3)], false);

        assert_eq!(
            stop_rx
                .try_recv()
                .expect("old session should begin draining"),
            HostSwitchStopReason::Graceful,
        );
        assert!(state.slots.iter().any(|slot| slot.keyboard() == &route(1)));
    }

    #[test]
    fn stale_link_invalidates_transition_intent() {
        let mut state = HostSwitchManagerState::new();
        state.transition = Some(TransitionPhase::Waiting(TransitionIntent {
            link: link(2),
            host: 1,
            source: TransitionSource::KeyPress,
        }));

        state.reconcile_transition(&[link(3)], false);
        assert!(state.transition.is_none());
        assert!(state.begin_transition(false).is_none());
    }

    #[test]
    fn restoring_firmware_blocks_a_changed_link_for_the_same_keyboard() {
        let mut state = HostSwitchManagerState::new();
        state.slots.push(HostSwitchSlot::Recovering(Recovery {
            link: link(2),
            generation: 1,
            requested_host: None,
            restore: RestorePhase::Restoring,
        }));

        assert!(
            state.owns_keyboard(&link(3).keyboard),
            "target changes must not permit re-arm over pending keyboard firmware"
        );
    }

    #[test]
    fn terminal_completion_waits_for_restore_acknowledgement() {
        let mut state = HostSwitchManagerState::new();
        state.slots.push(HostSwitchSlot::Recovering(Recovery {
            link: link(2),
            generation: 1,
            requested_host: None,
            restore: RestorePhase::Restoring,
        }));

        assert!(state.terminal_completion(true).is_none());
        state.handle_restore_completion(
            RestoreCompletion {
                generation: 0,
                result: Ok(HostSwitchRestoreOutcome::Restored),
            },
            &[],
            true,
        );
        assert!(
            state.terminal_completion(true).is_none(),
            "stale completion cannot discard recovery"
        );
        state.handle_restore_completion(
            RestoreCompletion {
                generation: 1,
                result: Ok(HostSwitchRestoreOutcome::Restored),
            },
            &[],
            true,
        );
        assert!(matches!(
            state.terminal_completion(true),
            Some(ManagerCompletion::Graceful)
        ));
    }

    #[test]
    fn transition_waits_for_restoration_and_keeps_running_until_acknowledged() {
        let mut state = HostSwitchManagerState::new();
        state.slots.push(HostSwitchSlot::Recovering(Recovery {
            link: link(2),
            generation: 1,
            requested_host: None,
            restore: RestorePhase::Restoring,
        }));
        state.transition = Some(TransitionPhase::Waiting(TransitionIntent {
            link: link(2),
            host: 2,
            source: TransitionSource::KeyPress,
        }));
        assert!(state.begin_transition(false).is_none());
        assert!(matches!(
            state.transition,
            Some(TransitionPhase::Waiting(_))
        ));

        state.handle_restore_completion(
            RestoreCompletion {
                generation: 1,
                result: Ok(HostSwitchRestoreOutcome::Restored),
            },
            &[link(2)],
            false,
        );
        assert_eq!(state.begin_transition(false).unwrap().host, 2);
        // Another manager wake while switching must not remove Running.
        assert!(state.begin_transition(false).is_none());
        assert!(state.terminal_completion(true).is_none());
        handle_manager_event(
            &mut state,
            ManagerEvent::Transition(Ok(TransitionOutcome::Settled)),
            &[],
            true,
        );
        assert!(matches!(
            state.terminal_completion(true),
            Some(ManagerCompletion::Graceful)
        ));
    }

    #[tokio::test]
    async fn completed_session_releases_receiver_lease_before_manager_acknowledgement() {
        let access = ReceiverAccess::default();
        let registry = ChannelRegistry::default();
        let (_signal, gate) = openlogi_hid::device_io_channel();
        let (events, mut received) = mpsc::unbounded_channel();
        let services = SessionServices {
            channel_pool: openlogi_hid::channel_pool(),
            registry,
            receiver_access: access.clone(),
            device_io: gate,
            events,
        };
        let _session = spawn_session(
            link(2),
            1,
            access.try_acquire_for_session().unwrap(),
            &services,
        );
        let _exclusive = tokio::time::timeout(
            Duration::from_secs(1),
            access.acquire_exclusive(ExclusiveAccessReason::Pairing),
        )
        .await
        .expect("the failed session must release its lease even before the manager consumes Done");
        let Some(ManagerEvent::Session(completion)) = received.recv().await else {
            panic!("expected session completion");
        };
        assert!(completion.result.unwrap().failed);
    }

    #[test]
    fn suspended_device_io_disables_retry_deadlines() {
        let retry_at = Instant::now() + RETRY_DELAY;
        let mut state = HostSwitchManagerState::new();
        state.slots.push(HostSwitchSlot::Restarting {
            link: link(2),
            retry_at,
        });

        assert_eq!(
            state.deadline(ReceiverRequestState::default(), true),
            Some(retry_at)
        );
        assert_eq!(state.deadline(ReceiverRequestState::default(), false), None);
    }

    fn requester_pair() -> (
        HostSwitchRequester,
        watch::Receiver<Option<HostRequest>>,
        watch::Sender<u64>,
    ) {
        let (asks, ask_rx) = watch::channel(None);
        let (settlements, settled_rx) = watch::channel(0);
        (
            HostSwitchRequester {
                asks: Arc::new(asks),
                settled: settled_rx,
            },
            ask_rx,
            settlements,
        )
    }

    #[tokio::test]
    async fn an_ask_only_resolves_once_the_manager_settles_it() {
        let (requester, asks, settlements) = requester_pair();
        let asking = tokio::spawn(async move { requester.request(3).await });
        tokio::task::yield_now().await;

        assert_eq!(*asks.borrow(), Some(HostRequest { serial: 1, host: 3 }));
        assert!(
            !asking.is_finished(),
            "a caller parked until the transition ends is what makes its own \
             cooldown cover the transition rather than only the ask"
        );

        settlements.send_replace(1);
        asking.await.expect("a settled ask should resolve");
    }

    #[tokio::test]
    async fn concurrent_asks_coalesce_into_the_latest_one() {
        let (requester, asks, settlements) = requester_pair();
        let first = tokio::spawn({
            let requester = requester.clone();
            async move { requester.request(2).await }
        });
        tokio::task::yield_now().await;
        let second = tokio::spawn(async move { requester.request(3).await });
        tokio::task::yield_now().await;

        assert_eq!(
            *asks.borrow(),
            Some(HostRequest { serial: 2, host: 3 }),
            "only the latest ask may survive; a stale host move is not work",
        );

        // Settling the newer serial releases the superseded caller too.
        settlements.send_replace(2);
        first.await.expect("the superseded ask should resolve");
        second.await.expect("the latest ask should resolve");
    }

    #[tokio::test]
    async fn a_departed_manager_releases_a_waiting_ask() {
        let (requester, _asks, settlements) = requester_pair();
        let asking = tokio::spawn(async move { requester.request(2).await });
        tokio::task::yield_now().await;

        drop(settlements);
        asking
            .await
            .expect("a caller must not hang when the manager is gone");
    }

    #[test]
    fn an_ask_moves_every_published_link_through_the_one_transition_slot() {
        let now = Instant::now();
        let published = [link(2), link(3)];
        let mut state = HostSwitchManagerState::new();

        state.accept_request(HostRequest { serial: 1, host: 3 }, &published, now);
        state.promote_request(&published, false, now);
        let first = state.begin_transition(false).expect("the ask should run");
        assert_eq!(first.host, 3);
        assert!(
            state.begin_transition(false).is_none(),
            "the manager stays the single transition authority",
        );

        handle_manager_event(
            &mut state,
            ManagerEvent::Transition(Ok(TransitionOutcome::Settled)),
            &published,
            false,
        );
        state.promote_request(&published, false, now);
        let second = state
            .begin_transition(false)
            .expect("the second link should move");
        assert_eq!(second.host, 3);
        assert_eq!(
            state.settled, 0,
            "the ask is not settled while work remains"
        );

        handle_manager_event(
            &mut state,
            ManagerEvent::Transition(Ok(TransitionOutcome::Settled)),
            &published,
            false,
        );
        state.promote_request(&published, false, now);
        assert!(state.request.is_none());
        assert_eq!(
            state.settled, 1,
            "the caller is released once nothing is left to move"
        );
    }

    #[test]
    fn re_reading_the_published_ask_does_not_restart_it() {
        let ask = HostRequest { serial: 1, host: 2 };
        let now = Instant::now();
        let mut state = HostSwitchManagerState::new();

        state.accept_request(ask, &[link(2)], now);
        state.promote_request(&[link(2)], false, now);
        assert_eq!(
            state
                .begin_transition(false)
                .expect("the ask should run")
                .host,
            2
        );

        // The watch keeps holding the same value on every later manager wake.
        state.accept_request(ask, &[link(2)], now);
        assert_eq!(state.settled, 0);
        assert!(
            state
                .request
                .as_ref()
                .is_some_and(|request| request.serial == 1 && request.remaining.is_empty()),
            "an already-accepted ask must not be taken on twice",
        );
    }

    #[test]
    fn a_later_ask_supersedes_the_pending_one_and_releases_its_caller() {
        let now = Instant::now();
        let mut state = HostSwitchManagerState::new();

        state.accept_request(HostRequest { serial: 1, host: 2 }, &[link(2)], now);
        state.promote_request(&[link(2)], false, now);
        assert!(matches!(
            state.transition,
            Some(TransitionPhase::Waiting(_))
        ));

        state.accept_request(HostRequest { serial: 2, host: 3 }, &[link(2)], now);
        assert_eq!(
            state.settled, 1,
            "the superseded ask must release its caller"
        );
        assert!(
            state.transition.is_none(),
            "a queued intent for a superseded ask is a stale host move",
        );

        state.promote_request(&[link(2)], false, now);
        assert_eq!(
            state
                .begin_transition(false)
                .expect("the newest ask should run")
                .host,
            3
        );
    }

    #[test]
    fn a_superseded_asks_outcome_is_not_charged_to_its_successor() {
        let now = Instant::now();
        let mut state = HostSwitchManagerState::new();

        state.accept_request(HostRequest { serial: 1, host: 2 }, &[link(2)], now);
        state.promote_request(&[link(2)], false, now);
        let stale = state
            .begin_transition(false)
            .expect("the first ask should run")
            .source;

        state.accept_request(HostRequest { serial: 2, host: 2 }, &[link(2)], now);
        state.record_transition_outcome(stale, TransitionOutcome::Failed);

        assert!(
            state
                .request
                .as_ref()
                .is_some_and(|request| !request.failed),
            "the successor owns the slot and must not inherit the verdict",
        );
    }

    #[test]
    fn repeated_failures_throttle_the_next_ask_instead_of_taking_the_lease() {
        let now = Instant::now();
        let mut state = HostSwitchManagerState::new();

        state.accept_request(HostRequest { serial: 1, host: 2 }, &[link(2)], now);
        state.promote_request(&[link(2)], false, now);
        let intent = state.begin_transition(false).expect("the ask should run");
        assert_eq!(intent.host, 2);
        handle_manager_event(
            &mut state,
            ManagerEvent::Transition(Ok(TransitionOutcome::Failed)),
            &[link(2)],
            false,
        );
        state.promote_request(&[link(2)], false, now);
        assert_eq!(state.settled, 1);

        state.accept_request(HostRequest { serial: 2, host: 2 }, &[link(2)], now);
        assert!(
            state.request.is_none(),
            "a throttled ask must not cost another exclusive lease",
        );
        assert_eq!(
            state.settled, 2,
            "a throttled ask still releases its caller"
        );
        state.promote_request(&[link(2)], false, now);
        assert!(state.transition.is_none());

        // A different host was never the one refusing, so it is not throttled.
        state.accept_request(HostRequest { serial: 3, host: 3 }, &[link(2)], now);
        assert!(state.request.is_some());
    }

    #[test]
    fn each_consecutive_failure_doubles_the_dead_time_up_to_the_ceiling() {
        let now = Instant::now();
        let mut backoff = RequestBackoff::default();

        backoff.record(2, true, now);
        assert!(backoff.blocks(2, now + REQUEST_BACKOFF_BASE - Duration::from_millis(1)));
        assert!(!backoff.blocks(2, now + REQUEST_BACKOFF_BASE));
        assert!(
            !backoff.blocks(3, now),
            "only the refusing host is throttled"
        );

        backoff.record(2, true, now);
        assert!(backoff.blocks(2, now + REQUEST_BACKOFF_BASE));

        for _ in 0..32 {
            backoff.record(2, true, now);
        }
        assert!(
            !backoff.blocks(2, now + REQUEST_BACKOFF_MAX),
            "the dead time is capped so a later deliberate ask still runs",
        );

        backoff.record(2, false, now);
        assert!(!backoff.blocks(2, now), "a success clears the throttle");
    }

    #[test]
    fn a_terminal_manager_releases_the_outstanding_ask() {
        let now = Instant::now();
        let mut state = HostSwitchManagerState::new();

        state.accept_request(HostRequest { serial: 1, host: 2 }, &[link(2)], now);
        state.promote_request(&[link(2)], true, now);
        assert!(state.request.is_none());
        assert_eq!(state.settled, 1);
        assert!(state.transition.is_none());
        assert!(matches!(
            state.terminal_completion(true),
            Some(ManagerCompletion::Graceful)
        ));
    }

    #[tokio::test]
    async fn a_denied_session_lease_cannot_leave_a_due_deadline_behind() {
        // `reconcile_recoveries` leaves a due retry in place when it cannot take
        // a session lease, and `manage` short-circuits on a due deadline without
        // awaiting. Those two only stay safe together because a session lease is
        // refused exactly while an exclusive operation is queued or active,
        // which is when `deadline` reports nothing to wake for. Assert the
        // coupling here, so a future lease rule that breaks it fails as a test
        // rather than as a hot loop on a current-thread runtime.
        let access = ReceiverAccess::default();
        let mut requests = access.subscribe_requests();
        let retry_at = Instant::now() + RETRY_DELAY;
        let mut state = HostSwitchManagerState::new();
        state.slots.push(HostSwitchSlot::Restarting {
            link: link(2),
            retry_at,
        });

        assert!(access.try_acquire_for_session().is_some());
        assert_eq!(
            state.deadline(*requests.borrow_and_update(), true),
            Some(retry_at)
        );

        let _exclusive = access
            .acquire_exclusive(ExclusiveAccessReason::HostTransition)
            .await;
        assert!(access.try_acquire_for_session().is_none());
        assert_eq!(state.deadline(*requests.borrow_and_update(), true), None);
    }

    #[tokio::test(start_paused = true)]
    async fn departure_publication_finishes_wait_without_advancing_time() {
        let keyboard = route(1);
        let active = HostSwitchLink {
            keyboard: keyboard.clone(),
            targets: vec![route(2)],
        };
        let (links, mut published) = watch::channel(std::sync::Arc::new(vec![active]));
        let started = Instant::now();
        let waiting = tokio::spawn(async move {
            wait_for_departure(&mut published, &keyboard).await;
            Instant::now()
        });
        tokio::task::yield_now().await;

        links.send_replace(std::sync::Arc::new(Vec::new()));
        tokio::task::yield_now().await;

        assert_eq!(
            waiting.await.expect("departure waiter should finish"),
            started,
            "the link publication should reconcile departure immediately"
        );
    }
}
