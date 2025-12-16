//! Brutal congestion control algorithm
//!
//! A bandwidth-driven congestion control algorithm from Hysteria2 protocol,
//! designed for high packet loss network environments.

use std::any::Any;
use std::sync::Arc;

use super::{BASE_DATAGRAM_SIZE, Controller, ControllerFactory, ControllerMetrics, TransferableState};
use crate::connection::RttEstimator;
use crate::{Duration, Instant};

/// Number of sampling slots (seconds)
const PKT_INFO_SLOT_COUNT: usize = 5;
/// Minimum number of samples before rate adjustment
const MIN_SAMPLE_COUNT: u64 = 50;
/// Default minimum ACK rate (clamped floor)
const DEFAULT_MIN_ACK_RATE: f64 = 0.8;
/// Congestion window multiplier
const CWND_MULTIPLIER: f64 = 2.0;
/// Default initial RTT estimate (100ms, not matching QUIC spec(300))
const DEFAULT_INITIAL_RTT: Duration = Duration::from_millis(100);

/// Packet statistics for a time slot
#[derive(Debug, Clone, Default)]
struct PktInfo {
    /// Slot identifier (increments each second)
    slot_id: u64,
    /// Number of acknowledged packets
    ack_count: u64,
    /// Number of lost packets
    loss_count: u64,
}

/// Brutal congestion controller
///
/// A bandwidth-driven congestion control algorithm that maintains a user-specified
/// target bandwidth by dynamically adjusting the send rate based on ACK rate.
/// Unlike traditional congestion control algorithms, Brutal increases the send rate
/// when packet loss occurs to compensate and maintain throughput.
///
/// # Algorithm
///
/// The congestion window is calculated as:
/// ```text
/// CWND = (target_bps × RTT × multiplier) / ack_rate
/// ```
///
/// Where `ack_rate` is the ratio of acknowledged packets to total packets sent,
/// sampled over a sliding window of 5 seconds.
///
/// # Warning
///
/// This algorithm is **not TCP-friendly** and will aggressively consume bandwidth.
/// It should only be used in controlled environments where the available bandwidth
/// is known and dedicated.
#[derive(Debug, Clone)]
pub struct Brutal {
    config: Arc<BrutalConfig>,
    /// Current MTU
    current_mtu: u64,
    /// Statistics slots for tracking ACK/loss over time
    pkt_info_slots: [PktInfo; PKT_INFO_SLOT_COUNT],
    /// Current ACK rate (ratio of ACKed to total packets)
    ack_rate: f64,
    /// Last known RTT estimate
    last_rtt: Duration,
    /// Slot counter (increments roughly every second worth of RTTs)
    current_slot_id: u64,
    /// Base time for slot calculation
    base_time: Option<Instant>,
}

impl Brutal {
    /// Construct a new Brutal controller with the given configuration
    pub fn new(config: Arc<BrutalConfig>, current_mtu: u16) -> Self {
        Self {
            config,
            current_mtu: current_mtu as u64,
            pkt_info_slots: Default::default(),
            ack_rate: 1.0,
            last_rtt: DEFAULT_INITIAL_RTT,
            current_slot_id: 0,
            base_time: None,
        }
    }

    /// Get the current time slot index based on elapsed time
    fn get_slot_index(&mut self, now: Instant) -> usize {
        let base = *self.base_time.get_or_insert(now);
        let elapsed_secs = now.saturating_duration_since(base).as_secs();
        let slot_id = elapsed_secs;

        // Update slot ID if we've moved to a new second
        if slot_id != self.current_slot_id {
            self.current_slot_id = slot_id;
        }

        (slot_id % PKT_INFO_SLOT_COUNT as u64) as usize
    }

    /// Update ACK rate based on statistics from all slots except the current one
    fn update_ack_rate(&mut self, current_slot_index: usize) {
        let current_slot_id = self.current_slot_id;
        // Calculate minimum valid slot_id (slots older than this are stale)
        let min_slot_id = current_slot_id.saturating_sub(PKT_INFO_SLOT_COUNT as u64);
        let mut ack_count: u64 = 0;
        let mut loss_count: u64 = 0;

        for (i, slot) in self.pkt_info_slots.iter().enumerate() {
            // Skip current slot
            if i == current_slot_index {
                continue;
            }
            // Skip stale slots (older than PKT_INFO_SLOT_COUNT seconds)
            if slot.slot_id < min_slot_id {
                continue;
            }
            ack_count += slot.ack_count;
            loss_count += slot.loss_count;
        }

        let total = ack_count + loss_count;
        if total < MIN_SAMPLE_COUNT {
            // Not enough samples, assume perfect delivery
            self.ack_rate = 1.0;
            return;
        }

        let rate = ack_count as f64 / total as f64;
        self.ack_rate = rate.max(self.config.min_ack_rate);
    }

    /// Record an ACK event in the current slot
    fn record_ack(&mut self, now: Instant) {
        let slot_index = self.get_slot_index(now);
        let slot_id = self.current_slot_id;

        let slot = &mut self.pkt_info_slots[slot_index];
        if slot.slot_id != slot_id {
            // New slot, reset counters
            *slot = PktInfo {
                slot_id,
                ack_count: 1,
                loss_count: 0,
            };
        } else {
            slot.ack_count += 1;
        }

        self.update_ack_rate(slot_index);
    }

    /// Record a loss event in the current slot
    fn record_loss(&mut self, now: Instant) {
        let slot_index = self.get_slot_index(now);
        let slot_id = self.current_slot_id;

        let slot = &mut self.pkt_info_slots[slot_index];
        if slot.slot_id != slot_id {
            // New slot, reset counters
            *slot = PktInfo {
                slot_id,
                ack_count: 0,
                loss_count: 1,
            };
        } else {
            slot.loss_count += 1;
        }

        self.update_ack_rate(slot_index);
    }

    /// Calculate the congestion window based on target bandwidth and ACK rate
    fn calculate_cwnd(&self) -> u64 {
        let rtt_secs = self.last_rtt.as_secs_f64();
        if rtt_secs <= 0.0 {
            return self.config.initial_window;
        }

        let target_bps = self.config.target_bps as f64;
        // CWND = (BPS × RTT × multiplier) / ack_rate
        let cwnd = (target_bps * rtt_secs * CWND_MULTIPLIER / self.ack_rate) as u64;

        cwnd.max(self.minimum_window())
    }

    /// Calculate the pacing rate in bits per second
    fn pacing_rate(&self) -> u64 {
        // Adjust rate based on ACK rate and convert to bits/s
        let adjusted_bps = self.config.target_bps as f64 / self.ack_rate;
        (adjusted_bps * 8.0) as u64
    }

    /// Minimum congestion window (4 × MTU)
    fn minimum_window(&self) -> u64 {
        4 * self.current_mtu
    }
}

impl Controller for Brutal {
    fn on_ack(
        &mut self,
        now: Instant,
        _sent: Instant,
        _bytes: u64,
        _app_limited: bool,
        rtt: &RttEstimator,
    ) {
        // Update RTT estimate
        self.last_rtt = rtt.get();

        // Record the ACK
        self.record_ack(now);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
        // Record the loss - Brutal compensates for loss by increasing send rate
        self.record_loss(now);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu as u64;
    }

    fn window(&self) -> u64 {
        self.calculate_cwnd()
    }

    fn metrics(&self) -> ControllerMetrics {
        ControllerMetrics {
            congestion_window: self.window(),
            ssthresh: None,
            pacing_rate: Some(self.pacing_rate()),
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_window
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn transferable_state(&self) -> TransferableState {
        TransferableState {
            congestion_window: self.window(),
            ssthresh: None,
            in_recovery: false,
        }
    }

    fn apply_transferred_state(&mut self, _state: &TransferableState) -> bool {
        // Brutal is primarily driven by the configured target_bps,
        // reset statistics and let it rediscover the optimal rate
        self.pkt_info_slots = Default::default();
        self.ack_rate = 1.0;
        self.current_slot_id = 0;
        self.base_time = None;
        true
    }

    fn name(&self) -> &'static str {
        "brutal"
    }
}

/// Configuration for the [`Brutal`] congestion controller
///
/// # Example
///
/// ```
/// use quinn_proto::congestion::BrutalConfig;
///
/// // Create a configuration for 100 Mbps target bandwidth
/// let config = BrutalConfig::new(100);
/// ```
#[derive(Debug, Clone)]
pub struct BrutalConfig {
    /// Target bandwidth in bytes per second
    pub(crate) target_bps: u64,
    /// Initial congestion window
    pub(crate) initial_window: u64,
    /// Minimum ACK rate (floor for rate adjustment)
    pub(crate) min_ack_rate: f64,
}

impl BrutalConfig {
    /// Create a new configuration with the specified target bandwidth
    ///
    /// # Arguments
    ///
    /// * `target_mbps` - Target bandwidth in megabits per second (Mbps)
    ///
    /// # Example
    ///
    /// ```
    /// use quinn_proto::congestion::BrutalConfig;
    ///
    /// let config = BrutalConfig::new(100); // 100 Mbps
    /// ```
    pub fn new(target_mbps: u64) -> Self {
        Self {
            target_bps: target_mbps * 1_000_000 / 8, // Mbps -> Bytes/s
            initial_window: 14720.clamp(2 * BASE_DATAGRAM_SIZE, 10 * BASE_DATAGRAM_SIZE),
            min_ack_rate: DEFAULT_MIN_ACK_RATE,
        }
    }

    /// Set the target bandwidth in bytes per second
    pub fn target_bps(&mut self, bps: u64) -> &mut Self {
        self.target_bps = bps;
        self
    }

    /// Set the target bandwidth in megabits per second
    pub fn target_mbps(&mut self, mbps: u64) -> &mut Self {
        self.target_bps = mbps * 1_000_000 / 8;
        self
    }

    /// Set the initial congestion window in bytes
    ///
    /// Recommended value: `min(10 * max_datagram_size, max(2 * max_datagram_size, 14720))`
    pub fn initial_window(&mut self, value: u64) -> &mut Self {
        self.initial_window = value;
        self
    }

    /// Set the minimum ACK rate
    ///
    /// This is the floor for ACK rate adjustment. When the actual ACK rate falls
    /// below this value, it will be clamped to this minimum. Lower values allow
    /// more aggressive compensation for packet loss.
    ///
    /// Default: 0.8 (80%)
    /// Valid range: 0.1 to 1.0
    pub fn min_ack_rate(&mut self, rate: f64) -> &mut Self {
        self.min_ack_rate = rate.clamp(0.1, 1.0);
        self
    }
}

impl Default for BrutalConfig {
    fn default() -> Self {
        Self {
            target_bps: 100 * 1_000_000 / 8, // Default 100 Mbps (matching Quinn default)
            initial_window: 14720.clamp(2 * BASE_DATAGRAM_SIZE, 10 * BASE_DATAGRAM_SIZE),
            min_ack_rate: DEFAULT_MIN_ACK_RATE,
        }
    }
}

impl ControllerFactory for BrutalConfig {
    fn build(self: Arc<Self>, _now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Brutal::new(self, current_mtu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(target_mbps: u64) -> Arc<BrutalConfig> {
        Arc::new(BrutalConfig::new(target_mbps))
    }

    #[test]
    fn test_config_conversion() {
        let config = BrutalConfig::new(100); // 100 Mbps
        // 100 Mbps = 100 * 1_000_000 / 8 = 12_500_000 bytes/s
        assert_eq!(config.target_bps, 12_500_000);
    }

    #[test]
    fn test_config_builder() {
        let mut config = BrutalConfig::default();
        config.target_mbps(200).min_ack_rate(0.5);
        assert_eq!(config.target_bps, 200 * 1_000_000 / 8);
        assert!((config.min_ack_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_min_ack_rate_clamping() {
        let mut config = BrutalConfig::default();
        config.min_ack_rate(0.05); // Below minimum
        assert!((config.min_ack_rate - 0.1).abs() < f64::EPSILON);

        config.min_ack_rate(1.5); // Above maximum
        assert!((config.min_ack_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_initial_ack_rate() {
        let config = make_config(100);
        let brutal = Brutal::new(config, 1200);
        assert!((brutal.ack_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_minimum_window() {
        let config = make_config(100);
        let brutal = Brutal::new(config, 1200);
        assert_eq!(brutal.minimum_window(), 4 * 1200);
    }

    #[test]
    fn test_cwnd_calculation() {
        let config = make_config(100);
        let mut brutal = Brutal::new(config, 1200);

        // With default RTT of 100ms and ack_rate of 1.0:
        // CWND = (12_500_000 * 0.1 * 2) / 1.0 = 2_500_000 bytes
        let cwnd = brutal.calculate_cwnd();
        assert!(cwnd >= 2_000_000, "CWND should be approximately 2.5MB");

        // With lower ACK rate, CWND should increase
        brutal.ack_rate = 0.8;
        let cwnd_with_loss = brutal.calculate_cwnd();
        assert!(
            cwnd_with_loss > cwnd,
            "CWND should increase when ACK rate decreases"
        );
    }

    #[test]
    fn test_pacing_rate() {
        let config = make_config(100);
        let mut brutal = Brutal::new(config, 1200);

        // With ack_rate of 1.0:
        // pacing_rate = (12_500_000 / 1.0) * 8 = 100_000_000 bits/s = 100 Mbps
        let rate = brutal.pacing_rate();
        assert_eq!(rate, 100_000_000);

        // With lower ACK rate, pacing rate should increase
        brutal.ack_rate = 0.8;
        let rate_with_loss = brutal.pacing_rate();
        assert!(
            rate_with_loss > rate,
            "Pacing rate should increase when ACK rate decreases"
        );
    }

    #[test]
    fn test_controller_name() {
        let config = make_config(100);
        let brutal = Brutal::new(config, 1200);
        assert_eq!(brutal.name(), "brutal");
    }
}
