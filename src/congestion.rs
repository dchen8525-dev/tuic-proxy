//! Congestion-control choices for lossy transit paths.
//!
//! Quic's default controller (CUBIC) treats every lost UDP packet as
//! congestion and halves the window.  On a path that drops a roughly constant
//! share of packets -- an international link at peak hours, a lossy mobile
//! network -- that response never lets the window recover, and a bulk download
//! collapses to a small fraction of the path's capacity.
//!
//! Two loss-tolerant options are provided:
//!
//! * [`Congestion::Bbr`] uses quinn's BBR implementation (initial window
//!   200 packets ≈ 240 KiB), which paces against the bandwidth it has
//!   observed instead of cutting the window on loss.  The server-side
//!   anti-amplification limit still applies before the path is validated.
//! * [`Congestion::LossTolerant`] wraps BBR and additionally prevents
//!   ordinary loss from shrinking the in-flight window.  This is the closest
//!   equivalent of FCP's fixed `--window` behaviour: retransmissions still
//!   repair lost packets, but the pipe stays full in the meantime.
//!
//! Both are local choices; the peer does not need to agree on one, so old
//! clients and servers keep working unchanged.  The default stays CUBIC.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use quinn::congestion::{Bbr, BbrConfig, Controller, ControllerFactory, CubicConfig};
use quinn_proto::RttEstimator;

/// Nominal QUIC datagram size used to derive minimum windows.
const BASE_DATAGRAM_SIZE: u64 = 1200;

/// Defer to the inner controller once its window drops to this multiple of
/// the MTU.
///
/// BBR's ProbeRtt phase asks for a ~4-packet window to refresh its min-RTT
/// estimate, and a genuinely collapsed controller sits at its minimum.  In
/// both cases holding the pipe full at the historical high-water mark would
/// defeat the measurement and amplify real congestion, so the wrapper steps
/// aside until the inner window recovers.
const MIN_WINDOW_DELEGATION: u64 = 8;

/// Congestion controller selected on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Congestion {
    /// Quic's default loss-based controller (CUBIC).
    #[default]
    Cubic,
    /// BBR: bandwidth-based pacing, insensitive to isolated loss.
    Bbr,
    /// BBR whose window never shrinks on ordinary loss.
    LossTolerant,
}

impl Congestion {
    /// Parse a CLI value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cubic" => Some(Self::Cubic),
            "bbr" => Some(Self::Bbr),
            "loss-tolerant" | "loss_tolerant" => Some(Self::LossTolerant),
            _ => None,
        }
    }

    /// `clap` value parser for `--congestion`.
    pub fn clap_parse(value: &str) -> Result<Self, String> {
        Self::parse(value).ok_or_else(|| {
            format!("unknown congestion controller {value:?} (cubic, bbr, loss-tolerant)")
        })
    }

    /// Stable name used in logs and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cubic => "cubic",
            Self::Bbr => "bbr",
            Self::LossTolerant => "loss-tolerant",
        }
    }

    /// Build the controller factory for a QUIC transport configuration.
    pub fn factory(self) -> Arc<dyn ControllerFactory + Send + Sync> {
        match self {
            Self::Cubic => Arc::new(CubicConfig::default()),
            Self::Bbr => Arc::new(BbrConfig::default()),
            Self::LossTolerant => Arc::new(LossTolerantFactory {
                bbr: BbrConfig::default(),
            }),
        }
    }
}

impl std::fmt::Display for Congestion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// [`BbrConfig`] extended with a wrapper that holds the window.
///
/// `BbrConfig` itself builds a [`Bbr`](quinn::congestion::Bbr) instance; this
/// factory intercepts construction so the wrapper is transparent to quinn.
#[derive(Debug, Clone)]
struct LossTolerantFactory {
    bbr: BbrConfig,
}

impl ControllerFactory for LossTolerantFactory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        let bbr = Arc::new(self.bbr.clone())
            .build(now, current_mtu)
            .into_any()
            .downcast::<Bbr>()
            .ok()
            .map(|boxed| *boxed);
        match bbr {
            Some(bbr) => Box::new(LossTolerant::new(bbr, current_mtu)),
            // `BbrConfig` always builds a `Bbr`; keep the raw controller if
            // that ever changes instead of failing.
            None => Arc::new(self.bbr.clone()).build(now, current_mtu),
        }
    }
}

/// Never let ordinary loss reduce the window.
///
/// Semantics, independent of what the wrapped controller does with loss:
///
/// * Ordinary loss events leave the window at its high-water mark ([`Self::held`]).
/// * Persistent congestion (every packet sent within a recovery period was
///   lost) pins the window to the RFC 9002 minimum (2 × MTU) until in-flight
///   drains below it, giving a genuinely congested path one quiet RTT.  Quinn's
///   BBR ignores the persistent-congestion flag entirely, so the wrapper
///   enforces the backoff itself.  Once in-flight drains, the window returns to
///   the high-water mark in one step -- that burst is the mode's trade-off for
///   never halving on loss.
/// * When the inner controller asks for a near-minimum window (ProbeRtt or
///   collapse, see [`MIN_WINDOW_DELEGATION`]) the wrapper defers to it, keeping
///   BBR's min-RTT refresh working.
///
/// `clone_box` carries the high-water mark across a connection migration, the
/// same way quinn carries BBR state; a genuinely worse new path is only
/// corrected by persistent congestion.
#[derive(Debug, Clone)]
pub struct LossTolerant<C: Controller + Clone + 'static> {
    inner: C,
    /// Highest window the wrapper has ever held.
    held: u64,
    /// Current path MTU, tracked for the minimum-window calculations.
    current_mtu: u64,
    /// Active persistent-congestion floor: `window()` pins to this value
    /// until in-flight drains below it.
    floor: Option<u64>,
}

impl<C: Controller + Clone + 'static> LossTolerant<C> {
    /// Wrap `inner`, starting from its current window.  `current_mtu` is the
    /// MTU the inner controller was built with.
    pub fn new(inner: C, current_mtu: u16) -> Self {
        let held = inner.window();
        Self {
            inner,
            held,
            current_mtu: (u64::from(current_mtu)).max(BASE_DATAGRAM_SIZE),
            floor: None,
        }
    }
}

impl<C: Controller + Clone + 'static> Controller for LossTolerant<C> {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
        self.held = self.held.max(self.inner.window());
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
        self.held = self.held.max(self.inner.window());
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
        self.held = self.held.max(self.inner.window());
        if let Some(floor) = self.floor {
            if in_flight < floor {
                self.floor = None;
            }
        }
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
        if is_persistent_congestion {
            // Quinn's BBR ignores this flag; enforce the backoff here so a
            // genuinely congested path still gets one quiet RTT.
            self.floor = Some(2 * self.current_mtu);
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
        self.current_mtu = (u64::from(new_mtu)).max(BASE_DATAGRAM_SIZE);
        self.held = self.held.max(self.inner.window());
    }

    fn window(&self) -> u64 {
        if let Some(floor) = self.floor {
            return floor;
        }
        let inner_window = self.inner.window();
        if inner_window <= MIN_WINDOW_DELEGATION * self.current_mtu {
            return inner_window;
        }
        self.held.max(inner_window)
    }

    fn metrics(&self) -> quinn::congestion::ControllerMetrics {
        let mut metrics = self.inner.metrics();
        metrics.congestion_window = self.window();
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    /// A deterministic inner controller for exercising the wrapper.
    ///
    /// `shrink_on_loss` mimics NewReno (halve on loss); `false` mimics BBR,
    /// which ignores loss entirely.
    #[derive(Clone)]
    struct Stub {
        window: u64,
        mtu: u16,
        shrink_on_loss: bool,
    }

    impl Stub {
        fn bbr_like(window: u64) -> Self {
            Self {
                window,
                mtu: 1452,
                shrink_on_loss: false,
            }
        }

        fn new_reno_like(window: u64) -> Self {
            Self {
                window,
                mtu: 1452,
                shrink_on_loss: true,
            }
        }
    }

    impl Controller for Stub {
        fn on_congestion_event(
            &mut self,
            _now: Instant,
            _sent: Instant,
            is_persistent_congestion: bool,
            _lost_bytes: u64,
        ) {
            if self.shrink_on_loss {
                let reduction = if is_persistent_congestion { 4 } else { 2 };
                self.window = (self.window / reduction).max(2 * u64::from(self.mtu));
            }
        }

        fn on_mtu_update(&mut self, new_mtu: u16) {
            self.mtu = new_mtu;
        }

        fn window(&self) -> u64 {
            self.window
        }

        fn clone_box(&self) -> Box<dyn Controller> {
            Box::new(self.clone())
        }

        fn initial_window(&self) -> u64 {
            self.window
        }

        fn into_any(self: Box<Self>) -> Box<dyn Any> {
            self
        }
    }

    #[test]
    fn names_round_trip() {
        for mode in [Congestion::Cubic, Congestion::Bbr, Congestion::LossTolerant] {
            assert_eq!(Congestion::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(Congestion::parse("nope"), None);
        assert_eq!(Congestion::default(), Congestion::Cubic);
    }

    #[test]
    fn held_overrides_a_shrinking_inner_on_ordinary_loss() {
        let mut controller = LossTolerant::new(Stub::new_reno_like(1_000_000), 1452);
        controller.on_congestion_event(now(), now(), false, 1452);
        // The inner controller halved itself; the wrapper still holds 1 MB.
        assert_eq!(controller.window(), 1_000_000);
    }

    #[test]
    fn defers_to_the_inner_controller_near_the_minimum_window() {
        let mut controller = LossTolerant::new(Stub::bbr_like(1_000_000), 1452);
        // ProbeRtt-like: the inner controller asks for a 4-packet window.
        controller.inner.window = 4 * 1452;
        assert_eq!(controller.window(), 4 * 1452);
        // Back above the delegation threshold: the high-water mark applies.
        controller.inner.window = 100_000;
        assert_eq!(controller.window(), 1_000_000);
    }

    #[test]
    fn persistent_congestion_collapses_even_when_the_inner_ignores_it() {
        // BBR-like: the inner controller does nothing on loss.
        let mut controller = LossTolerant::new(Stub::bbr_like(1_000_000), 1452);
        controller.on_congestion_event(now(), now(), true, 100_000);
        assert_eq!(controller.window(), 2 * 1452);
    }

    #[test]
    fn persistent_congestion_floor_releases_once_in_flight_drains() {
        let mut controller = LossTolerant::new(Stub::bbr_like(1_000_000), 1452);
        controller.on_congestion_event(now(), now(), true, 100_000);
        assert_eq!(controller.window(), 2 * 1452);

        // Still above the floor: collapsed.
        controller.on_end_acks(now(), 100_000, false, None);
        assert_eq!(controller.window(), 2 * 1452);
        // Drained below the floor: released, back to the high-water mark.
        controller.on_end_acks(now(), 1_000, false, None);
        assert_eq!(controller.window(), 1_000_000);
    }

    #[test]
    fn acknowledgements_ratchet_the_held_window() {
        let mut controller = LossTolerant::new(Stub::bbr_like(1_000_000), 1452);
        controller.inner.window = 2_000_000;
        controller.on_sent(now(), 1452, 0);
        assert_eq!(controller.window(), 2_000_000);
        // Losing packets afterwards does not give the window back.
        controller.inner.window = 1_500_000;
        controller.on_congestion_event(now(), now(), false, 1452);
        assert_eq!(controller.window(), 2_000_000);
    }

    #[test]
    fn clone_box_carries_the_high_water_mark() {
        let mut controller = LossTolerant::new(Stub::bbr_like(1_000_000), 1452);
        controller.inner.window = 2_000_000;
        controller.on_sent(now(), 1452, 0);
        let cloned = controller.clone_box();
        assert_eq!(cloned.window(), 2_000_000);
    }

    #[test]
    fn mtu_updates_track_the_minimum_window() {
        let mut controller = LossTolerant::new(Stub::bbr_like(1_000_000), 1452);
        controller.on_mtu_update(1200);
        controller.on_congestion_event(now(), now(), true, 100_000);
        assert_eq!(controller.window(), 2 * 1200);
    }

    #[test]
    fn factories_build_a_usable_controller() {
        for mode in [Congestion::Cubic, Congestion::Bbr, Congestion::LossTolerant] {
            let controller = mode.factory().build(now(), 1452);
            assert!(controller.window() >= 2 * 1200, "{mode}");
            assert!(controller.initial_window() >= 2 * 1200, "{mode}");
        }
    }
}
