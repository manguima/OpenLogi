//! Move the linked devices to another host when the pointer leaves the desktop.
//!
//! Easy-Switch needs a key press; this watcher reads the pointer instead, so
//! walking off the right edge lands the keyboard and mouse on the machine that
//! sits to the right. The transition itself belongs to the host-switch manager
//! — this module only decides *when* and *where*, then asks.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use openlogi_core::config::{Edge, FlowConfig};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::watchers::host_switch::HostSwitchRequester;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// Read-only, coalescing view of the live `[flow]` section.
pub type FlowSettings = watch::Receiver<Arc<FlowConfig>>;

/// Inclusive bounding box of the desktop, in the OS's virtual-screen space.
///
/// The union of every monitor, so the edges tested are the outer edges of the
/// whole desktop rather than of whichever monitor the pointer is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) min_x: i32,
    pub(crate) min_y: i32,
    pub(crate) max_x: i32,
    pub(crate) max_y: i32,
}

/// One pointer observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) bounds: Bounds,
}

/// Why the pointer could not be read or moved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PointerError {
    /// No usable display server connection on this host.
    // Win32's cursor APIs are session-global, so that backend has no connect
    // step that can fail and never builds this variant.
    #[cfg_attr(
        target_os = "windows",
        expect(clippy::allow_attributes, reason = "see above"),
        allow(dead_code, reason = "constructed only by the X11 and macOS backends")
    )]
    #[error("no display server available: {0}")]
    Unavailable(String),
    /// The connection was established but a request failed.
    // The macOS backend is a stub whose `connect` never succeeds, so `sample`
    // and `warp` — the only operations that can fail a request — are
    // unreachable there and that backend never builds this variant.
    #[cfg_attr(
        target_os = "macos",
        expect(clippy::allow_attributes, reason = "see above"),
        allow(dead_code, reason = "constructed only by the X11 and Win32 backends")
    )]
    #[error("pointer request failed: {0}")]
    Request(String),
}

/// Which edge `sample` is touching, restricted to edges that lead somewhere.
fn edge_at(sample: Sample, config: &FlowConfig) -> Option<Edge> {
    Edge::ALL.into_iter().find(|&edge| {
        if config.edges.host_for(edge).is_none() {
            return false;
        }
        match edge {
            Edge::Left => sample.x <= sample.bounds.min_x,
            Edge::Right => sample.x >= sample.bounds.max_x,
            Edge::Top => sample.y <= sample.bounds.min_y,
            Edge::Bottom => sample.y >= sample.bounds.max_y,
        }
    })
}

/// Where to drop the pointer after a switch, pulled inward off the edge.
fn rebound_to(sample: Sample, edge: Edge, config: &FlowConfig) -> (i32, i32) {
    let back = i32::from(config.rebound_px);
    match edge {
        Edge::Left => (sample.bounds.min_x + back, sample.y),
        Edge::Right => (sample.bounds.max_x - back, sample.y),
        Edge::Top => (sample.x, sample.bounds.min_y + back),
        Edge::Bottom => (sample.x, sample.bounds.max_y - back),
    }
}

/// A dwell that completed and is now owed to the host-switch manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeSwitch {
    edge: Edge,
    host: u8,
}

/// Where the pointer stands relative to the edges that lead somewhere.
#[derive(Debug, Clone, Copy, Default)]
enum EdgeState {
    /// Not touching a live edge.
    #[default]
    Idle,
    /// Holding one edge since the recorded instant.
    Holding { edge: Edge, since: Instant },
    /// A switch was handed over and has not been reported settled yet.
    Switching,
    /// Settling after a switch; contacts are ignored until the instant passes.
    Cooling { until: Instant },
}

/// Sans-I/O dwell and cooldown state machine.
///
/// The cooldown is armed by [`Self::settled`], not by the observation that
/// produced the switch. Arming it at the ask would make the dead time a
/// property of how fast the watcher polls rather than of how long the hardware
/// takes, and a transition that outlasts the cooldown would then have a second
/// one queued behind it.
#[derive(Debug, Default)]
struct EdgeTracker {
    state: EdgeState,
}

impl EdgeTracker {
    /// Feed one observation; yields the switch owed once the dwell completes.
    ///
    /// The caller must report the returned switch back through
    /// [`Self::settled`]; until it does, further observations are inert.
    fn observe(
        &mut self,
        now: Instant,
        at: Option<Edge>,
        config: &FlowConfig,
    ) -> Option<EdgeSwitch> {
        match self.state {
            // The caller still owes a `settled`, so it is mid-transition.
            EdgeState::Switching => return None,
            EdgeState::Cooling { until } if now < until => return None,
            EdgeState::Idle | EdgeState::Holding { .. } | EdgeState::Cooling { .. } => {}
        }

        let Some(edge) = at else {
            // Leaving the edge forfeits the dwell; a later contact starts over.
            self.state = EdgeState::Idle;
            return None;
        };
        // Anything but an unbroken hold of this same edge starts a fresh dwell,
        // so a pointer parked through a cooldown owes the full dwell again
        // rather than firing the instant the cooldown lapses.
        let since = match self.state {
            EdgeState::Holding { edge: held, since } if held == edge => since,
            _ => {
                self.state = EdgeState::Holding { edge, since: now };
                now
            }
        };
        if now.duration_since(since) < Duration::from_millis(u64::from(config.dwell_ms)) {
            return None;
        }
        let host = config.edges.host_for(edge)?;
        self.state = EdgeState::Switching;
        Some(EdgeSwitch { edge, host })
    }

    /// Report that the switch handed out by [`Self::observe`] has finished, and
    /// start the cooldown from that moment.
    fn settled(&mut self, now: Instant, config: &FlowConfig) {
        self.state = EdgeState::Cooling {
            until: now + Duration::from_millis(u64::from(config.cooldown_ms)),
        };
    }

    /// Forget any partial dwell, e.g. after the config changed underneath.
    ///
    /// A cooldown is not a dwell and survives: the devices really did move.
    fn reset(&mut self) {
        if matches!(self.state, EdgeState::Holding { .. }) {
            self.state = EdgeState::Idle;
        }
    }
}

/// Spawn the pointer-edge watcher.
///
/// Owns a thread and a current-thread runtime, like the host-switch manager:
/// the pointer round-trips are short and blocking, and keeping them off the
/// shared runtime keeps a stalled display server from starving HID++ sessions.
pub fn spawn(flow: &FlowSettings, requester: HostSwitchRequester) {
    let mut flow = flow.clone();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                warn!(%error, "flow: could not build tokio runtime");
                return;
            }
        };
        runtime.block_on(watch_edges(&mut flow, &requester));
    });
}

async fn watch_edges(flow: &mut FlowSettings, requester: &HostSwitchRequester) {
    let mut pointer = None;
    let mut tracker = EdgeTracker::default();

    loop {
        let config = Arc::clone(&flow.borrow_and_update());
        if !config.is_active() {
            pointer = None;
            tracker.reset();
            // Nothing to poll — park until the section changes.
            if flow.changed().await.is_err() {
                return;
            }
            continue;
        }

        if pointer.is_none() {
            match platform::Pointer::connect() {
                Ok(connected) => pointer = Some(connected),
                Err(error) => {
                    warn!(%error, "flow: pointer unavailable — edge switching is off");
                    if flow.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
            }
        }
        let Some(active) = pointer.as_mut() else {
            continue;
        };

        match active.sample() {
            Ok(sample) => {
                let now = Instant::now();
                if let Some(switch) = tracker.observe(now, edge_at(sample, &config), &config) {
                    let (x, y) = rebound_to(sample, switch.edge, &config);
                    if let Err(error) = active.warp(x, y) {
                        debug!(%error, "flow: could not pull the pointer back");
                    }
                    debug!(
                        edge = ?switch.edge,
                        host = switch.host,
                        "flow: edge reached — requesting host switch"
                    );
                    // Park here for the whole transition. The manager coalesces
                    // asks, but a watcher that kept polling would still be
                    // asking on every frame the pointer stays on the edge; the
                    // one thing that keeps this loop from outrunning the
                    // hardware is that it cannot ask again until this returns.
                    requester.request(switch.host).await;
                    tracker.settled(Instant::now(), &config);
                }
            }
            Err(error) => {
                debug!(%error, "flow: pointer read failed — reconnecting");
                pointer = None;
                tracker.reset();
            }
        }

        tokio::time::sleep(config.poll_interval()).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use openlogi_core::config::{Edge, FlowConfig, FlowEdges};

    use super::{Bounds, EdgeSwitch, EdgeTracker, Sample, edge_at, rebound_to};

    fn switch(edge: Edge, host: u8) -> Option<EdgeSwitch> {
        Some(EdgeSwitch { edge, host })
    }

    const BOUNDS: Bounds = Bounds {
        min_x: 0,
        min_y: 0,
        max_x: 1919,
        max_y: 1079,
    };

    fn config() -> FlowConfig {
        FlowConfig {
            enabled: true,
            edges: FlowEdges {
                left: Some(1),
                right: Some(3),
                ..FlowEdges::default()
            },
            dwell_ms: 100,
            cooldown_ms: 1000,
            ..FlowConfig::default()
        }
    }

    fn at(x: i32, y: i32) -> Sample {
        Sample {
            x,
            y,
            bounds: BOUNDS,
        }
    }

    #[test]
    fn only_configured_edges_report_contact() {
        let config = config();
        assert_eq!(edge_at(at(0, 500), &config), Some(Edge::Left));
        assert_eq!(edge_at(at(1919, 500), &config), Some(Edge::Right));
        // Top is unset, so touching it is not an edge at all.
        assert_eq!(edge_at(at(900, 0), &config), None);
        assert_eq!(edge_at(at(900, 500), &config), None);
    }

    #[test]
    fn dwell_must_elapse_before_switching() {
        let config = config();
        let mut tracker = EdgeTracker::default();
        let start = Instant::now();
        assert_eq!(tracker.observe(start, Some(Edge::Right), &config), None);
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(50),
                Some(Edge::Right),
                &config
            ),
            None
        );
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(120),
                Some(Edge::Right),
                &config
            ),
            switch(Edge::Right, 3)
        );
    }

    #[test]
    fn leaving_the_edge_restarts_the_dwell() {
        let config = config();
        let mut tracker = EdgeTracker::default();
        let start = Instant::now();
        assert_eq!(tracker.observe(start, Some(Edge::Right), &config), None);
        assert_eq!(
            tracker.observe(start + Duration::from_millis(50), None, &config),
            None
        );
        // Back on the edge at 60ms: the original 50ms of dwell must not count.
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(60),
                Some(Edge::Right),
                &config
            ),
            None
        );
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(120),
                Some(Edge::Right),
                &config
            ),
            None
        );
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(170),
                Some(Edge::Right),
                &config
            ),
            switch(Edge::Right, 3)
        );
    }

    #[test]
    fn switching_edges_restarts_the_dwell() {
        let config = config();
        let mut tracker = EdgeTracker::default();
        let start = Instant::now();
        assert_eq!(tracker.observe(start, Some(Edge::Right), &config), None);
        assert_eq!(
            tracker.observe(start + Duration::from_millis(90), Some(Edge::Left), &config),
            None
        );
        // 110ms total, but only 20ms on the left edge.
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(110),
                Some(Edge::Left),
                &config
            ),
            None
        );
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(200),
                Some(Edge::Left),
                &config
            ),
            switch(Edge::Left, 1)
        );
    }

    #[test]
    fn a_held_edge_cannot_fire_again_until_the_switch_is_reported_settled() {
        let config = config();
        let mut tracker = EdgeTracker::default();
        let start = Instant::now();
        tracker.observe(start, Some(Edge::Right), &config);
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(100),
                Some(Edge::Right),
                &config
            ),
            switch(Edge::Right, 3)
        );
        // The transition is still running. However long it takes — the manager
        // budgets ten seconds for the device to leave — a pointer parked on the
        // edge must not accumulate a second move behind the first.
        for elapsed in [200, 1_000, 5_000, 30_000] {
            assert_eq!(
                tracker.observe(
                    start + Duration::from_millis(elapsed),
                    Some(Edge::Right),
                    &config
                ),
                None,
                "an unsettled switch must swallow every later contact",
            );
        }
        // Leaving and returning does not get around it either.
        assert_eq!(
            tracker.observe(start + Duration::from_millis(30_100), None, &config),
            None
        );
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(30_200),
                Some(Edge::Right),
                &config
            ),
            None
        );
    }

    #[test]
    fn the_cooldown_runs_from_the_settlement_not_from_the_ask() {
        let config = config();
        let mut tracker = EdgeTracker::default();
        let start = Instant::now();
        tracker.observe(start, Some(Edge::Right), &config);
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(100),
                Some(Edge::Right),
                &config
            ),
            switch(Edge::Right, 3)
        );

        // The transition took 4s; the 1s cooldown therefore runs to 5100ms, not
        // to 1100ms as it would if the ask had armed it.
        tracker.settled(start + Duration::from_millis(4_100), &config);
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(4_500),
                Some(Edge::Right),
                &config
            ),
            None
        );
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(5_050),
                Some(Edge::Right),
                &config
            ),
            None
        );
        // Past the cooldown a still-parked pointer owes a full fresh dwell,
        // so this contact only re-arms the timer.
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(5_200),
                Some(Edge::Right),
                &config
            ),
            None
        );
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(5_310),
                Some(Edge::Right),
                &config
            ),
            switch(Edge::Right, 3)
        );
    }

    #[test]
    fn a_reset_forfeits_a_partial_dwell_but_not_a_cooldown() {
        let config = config();
        let mut tracker = EdgeTracker::default();
        let start = Instant::now();

        tracker.observe(start, Some(Edge::Right), &config);
        tracker.reset();
        assert_eq!(
            tracker.observe(
                start + Duration::from_millis(110),
                Some(Edge::Right),
                &config
            ),
            None,
            "a forfeited dwell must start over",
        );

        let mut cooling = EdgeTracker::default();
        cooling.settled(start, &config);
        cooling.reset();
        assert_eq!(
            cooling.observe(
                start + Duration::from_millis(500),
                Some(Edge::Right),
                &config
            ),
            None,
            "the devices really did move, so the settle time still holds",
        );
    }

    #[test]
    fn rebound_pulls_inward_off_each_edge() {
        let config = FlowConfig {
            rebound_px: 10,
            ..config()
        };
        assert_eq!(rebound_to(at(0, 500), Edge::Left, &config), (10, 500));
        assert_eq!(rebound_to(at(1919, 500), Edge::Right, &config), (1909, 500));
        assert_eq!(rebound_to(at(900, 0), Edge::Top, &config), (900, 10));
        assert_eq!(
            rebound_to(at(900, 1079), Edge::Bottom, &config),
            (900, 1069)
        );
    }
}
