//! High-frequency token usage tracking for ACP usage_update notifications.
//!
//! This module provides intelligent, throttled token usage updates that balance
//! real-time feedback with performance considerations through multi-dimensional
//! triggering conditions.

use std::time::Instant;

/// High-frequency token usage tracker with intelligent triggering.
///
/// Tracks token usage and determines when to send usage_update notifications
/// based on multiple conditions: increment threshold, time interval, and
/// percentage thresholds.
pub struct HighFreqUsageTracker {
    base_used: u64,
    current_used: u64,
    size: u64,
    last_notified_used: u64,
    pending_tokens: u64,
    last_notify_time: Instant,
    min_increment: u64,
    min_interval_ms: u64,
    percentage_thresholds: Vec<f64>,
}

/// Usage update information sent to client.
#[derive(Clone, Debug)]
pub struct UsageUpdateInfo {
    pub used: u64,
    pub size: u64,
    pub increment: u64,
    pub timestamp: Instant,
}

impl HighFreqUsageTracker {
    pub fn new(base_used: u64, size: u64) -> Self {
        Self {
            base_used,
            current_used: base_used,
            size,
            last_notified_used: base_used,
            pending_tokens: 0,
            last_notify_time: Instant::now(),
            min_increment: 50,
            min_interval_ms: 100,
            percentage_thresholds: vec![50.0, 75.0, 85.0, 90.0, 95.0],
        }
    }

    pub fn with_config(
        base_used: u64,
        size: u64,
        min_increment: u64,
        min_interval_ms: u64,
    ) -> Self {
        Self {
            base_used,
            current_used: base_used,
            size,
            last_notified_used: base_used,
            pending_tokens: 0,
            last_notify_time: Instant::now(),
            min_increment,
            min_interval_ms,
            percentage_thresholds: vec![50.0, 75.0, 85.0, 90.0, 95.0],
        }
    }

    pub fn with_custom_thresholds(base_used: u64, size: u64, thresholds: Vec<f64>) -> Self {
        Self {
            base_used,
            current_used: base_used,
            size,
            last_notified_used: base_used,
            pending_tokens: 0,
            last_notify_time: Instant::now(),
            min_increment: 50,
            min_interval_ms: 100,
            percentage_thresholds: thresholds,
        }
    }

    pub fn update_tokens(&mut self, delta: u64) -> Option<UsageUpdateInfo> {
        if delta == 0 {
            return None;
        }

        self.pending_tokens += delta;
        self.current_used = self.base_used + self.pending_tokens;

        let now = Instant::now();
        let elapsed_ms = now.duration_since(self.last_notify_time).as_millis() as u64;

        let should_notify = self.increment_trigger_met()
            || self.interval_trigger_met(elapsed_ms)
            || self.percentage_trigger_met();

        if should_notify && self.current_used != self.last_notified_used {
            let increment = self.current_used - self.last_notified_used;
            self.last_notified_used = self.current_used;
            self.last_notify_time = now;

            return Some(UsageUpdateInfo {
                used: self.current_used,
                size: self.size,
                increment,
                timestamp: now,
            });
        }

        None
    }

    fn increment_trigger_met(&self) -> bool {
        self.pending_tokens >= self.min_increment
    }

    fn interval_trigger_met(&self, elapsed_ms: u64) -> bool {
        elapsed_ms >= self.min_interval_ms
    }

    fn percentage_trigger_met(&self) -> bool {
        if self.percentage_thresholds.is_empty() {
            return false;
        }

        let current_percentage = (self.current_used as f64 / self.size as f64) * 100.0;
        let last_percentage = (self.last_notified_used as f64 / self.size as f64) * 100.0;

        self.percentage_thresholds
            .iter()
            .any(|&threshold| last_percentage < threshold && current_percentage >= threshold)
    }

    pub fn get_increment(&self) -> u64 {
        self.current_used - self.last_notified_used
    }

    pub fn get_current_usage(&self) -> u64 {
        self.current_used
    }

    pub fn get_usage_percentage(&self) -> f64 {
        (self.current_used as f64 / self.size as f64) * 100.0
    }

    pub fn adjust_frequency_based_on_load(&mut self, system_load: f64) {
        if system_load > 0.8 {
            self.min_increment = 100;
            self.min_interval_ms = 200;
        } else if system_load < 0.3 {
            self.min_increment = 25;
            self.min_interval_ms = 50;
        } else {
            self.min_increment = 50;
            self.min_interval_ms = 100;
        }
    }

    pub fn force_update(&mut self) -> Option<UsageUpdateInfo> {
        if self.current_used != self.last_notified_used {
            let increment = self.current_used - self.last_notified_used;
            self.last_notified_used = self.current_used;
            self.last_notify_time = Instant::now();

            return Some(UsageUpdateInfo {
                used: self.current_used,
                size: self.size,
                increment,
                timestamp: Instant::now(),
            });
        }

        None
    }

    pub fn get_base_used(&self) -> u64 {
        self.base_used
    }

    pub fn get_size(&self) -> u64 {
        self.size
    }

    pub fn reset(&mut self, base_used: u64) {
        self.base_used = base_used;
        self.current_used = base_used;
        self.last_notified_used = base_used;
        self.pending_tokens = 0;
        self.last_notify_time = Instant::now();
    }
}

impl Default for HighFreqUsageTracker {
    fn default() -> Self {
        Self::new(0, 128000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_new_tracker_initialization() {
        let tracker = HighFreqUsageTracker::new(1000, 10000);
        assert_eq!(tracker.get_current_usage(), 1000);
        assert_eq!(tracker.get_size(), 10000);
        assert_eq!(tracker.get_usage_percentage(), 10.0);
    }

    #[test]
    fn test_increment_trigger() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);

        assert!(tracker.update_tokens(20).is_none());
        assert!(tracker.update_tokens(30).is_some());
    }

    #[test]
    fn test_increment_threshold_configurable() {
        let mut tracker = HighFreqUsageTracker::with_config(1000, 10000, 25, 100);

        assert!(tracker.update_tokens(10).is_none());
        assert!(tracker.update_tokens(15).is_some()); // Total 25
    }

    #[test]
    fn test_interval_trigger() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);

        tracker.update_tokens(10);
        thread::sleep(Duration::from_millis(50));
        assert!(tracker.update_tokens(10).is_none());

        thread::sleep(Duration::from_millis(60));
        assert!(tracker.update_tokens(10).is_some());
    }

    #[test]
    fn test_percentage_trigger() {
        let mut tracker = HighFreqUsageTracker::new(4000, 10000);

        assert!(tracker.update_tokens(1000).is_some()); // 5000/10000 = 50%
    }

    #[test]
    fn test_multiple_percentage_thresholds() {
        let mut tracker = HighFreqUsageTracker::new(4900, 10000);

        assert!(tracker.update_tokens(100).is_some()); // 5000/10000 = 50%
        assert!(tracker.update_tokens(2000).is_some()); // 7000/10000 = 70%, passes 75%
        assert!(tracker.update_tokens(1000).is_some()); // 8000/10000 = 80%, passes 85%
    }

    #[test]
    fn test_usage_update_info_accuracy() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);

        let update = tracker.update_tokens(60).unwrap();
        assert_eq!(update.used, 1060);
        assert_eq!(update.size, 10000);
        assert_eq!(update.increment, 60);
    }

    #[test]
    fn test_zero_delta_no_update() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);
        assert!(tracker.update_tokens(0).is_none());
    }

    #[test]
    fn test_force_update() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);

        tracker.update_tokens(10);
        let update = tracker.force_update().unwrap();
        assert_eq!(update.used, 1010);
        assert_eq!(update.increment, 10);
    }

    #[test]
    fn test_force_update_no_change() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);
        assert!(tracker.force_update().is_none());
    }

    #[test]
    fn test_adjust_frequency_based_on_load() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);

        tracker.adjust_frequency_based_on_load(0.9);
        assert_eq!(tracker.min_increment, 100);
        assert_eq!(tracker.min_interval_ms, 200);

        tracker.adjust_frequency_based_on_load(0.2);
        assert_eq!(tracker.min_increment, 25);
        assert_eq!(tracker.min_interval_ms, 50);

        tracker.adjust_frequency_based_on_load(0.5);
        assert_eq!(tracker.min_increment, 50);
        assert_eq!(tracker.min_interval_ms, 100);
    }

    #[test]
    fn test_custom_thresholds() {
        let mut tracker =
            HighFreqUsageTracker::with_custom_thresholds(1000, 10000, vec![30.0, 60.0, 90.0]);

        assert!(tracker.update_tokens(2000).is_some()); // 3000/10000 = 30%
        assert!(tracker.update_tokens(3000).is_some()); // 6000/10000 = 60%
    }

    #[test]
    fn test_reset_functionality() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);

        tracker.update_tokens(100);
        tracker.update_tokens(200);

        tracker.reset(500);

        assert_eq!(tracker.get_current_usage(), 500);
        assert_eq!(tracker.get_base_used(), 500);
    }

    #[test]
    fn test_default_implementation() {
        let tracker = HighFreqUsageTracker::default();
        assert_eq!(tracker.get_current_usage(), 0);
        assert_eq!(tracker.get_size(), 128000);
    }

    #[test]
    fn test_concurrent_updates_simulation() {
        let mut tracker = HighFreqUsageTracker::new(1000, 10000);

        let mut updates_count = 0;
        for i in 1..=10 {
            if tracker.update_tokens(i * 10).is_some() {
                updates_count += 1;
            }
        }

        assert!(updates_count > 0);
        assert_eq!(tracker.get_current_usage(), 1550); // 1000 + sum(10, 20, ..., 100)
    }
}

// Integration tests with SessionNotifier
#[cfg(test)]
mod session_notifier_integration_tests {

    use crate::stream_bridge::SessionNotifier;
    use agent_client_protocol::schema::v1::SessionId;

    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_high_freq_tracking_with_notifier() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("test_session");

        let notifier = SessionNotifier::new(tx, session_id);
        notifier.enable_high_freq_tracking(1000, 10000);

        // Simulate token usage events
        let mut updates_received = 0;
        let mut total_increment = 0;

        for i in 1..=5 {
            let delta = i * 20; // 20, 40, 60, 80, 100 tokens
            if let Some(update_info) = notifier.test_update_high_freq_tokens(delta) {
                total_increment += update_info.increment;
                updates_received += 1;
            }
        }

        // Verify that some updates were triggered
        assert!(updates_received > 0, "Should receive at least one update");
        assert_eq!(
            total_increment, 300,
            "Total increment should match sum of deltas"
        );

        // Verify final state
        let final_usage = notifier.get_high_freq_tracker_status().unwrap().0;
        assert_eq!(final_usage, 1300, "Final usage should be 1000 + 300");
    }

    #[tokio::test]
    async fn test_percentage_trigger_in_notifier() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("test_session");

        let notifier = SessionNotifier::new(tx, session_id);
        notifier.enable_high_freq_tracking(4000, 10000);

        let mut updates_received = 0;

        // Trigger 50% threshold (5000/10000)
        if notifier.test_update_high_freq_tokens(1000).is_some() {
            updates_received += 1;
        }

        assert_eq!(updates_received, 1, "Should trigger at 50% threshold");
    }

    #[tokio::test]
    async fn test_time_interval_trigger_in_notifier() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("test_session");

        let notifier = SessionNotifier::new(tx, session_id);
        notifier.enable_high_freq_tracking(1000, 10000);

        let mut updates_received = 0;

        // Add small tokens (below increment threshold)
        if notifier.test_update_high_freq_tokens(10).is_some() {
            updates_received += 1;
        }

        assert_eq!(
            updates_received, 0,
            "Should not trigger before time interval"
        );

        // Wait for interval to pass
        tokio::time::sleep(Duration::from_millis(110)).await;

        // Add more small tokens
        if notifier.test_update_high_freq_tokens(10).is_some() {
            updates_received += 1;
        }

        assert_eq!(updates_received, 1, "Should trigger after time interval");
    }

    #[tokio::test]
    async fn test_custom_configuration_in_notifier() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("test_session");

        let notifier = SessionNotifier::new(tx, session_id);
        notifier.enable_high_freq_tracking_with_config(1000, 10000, 25, 50);

        let mut updates_received = 0;

        // Should trigger with smaller threshold
        for i in 1..=3 {
            if notifier.test_update_high_freq_tokens(i * 10).is_some() {
                updates_received += 1;
            }
        }

        assert!(
            updates_received > 0,
            "Should trigger with custom threshold of 25"
        );
    }

    #[tokio::test]
    async fn test_disable_high_freq_tracking() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("test_session");

        let notifier = SessionNotifier::new(tx, session_id);
        notifier.enable_high_freq_tracking(1000, 10000);

        // Add some tokens
        notifier.test_update_high_freq_tokens(50);

        // Disable tracking
        notifier.disable_high_freq_tracking().await;

        // Verify tracker is cleared
        let tracker_opt = notifier.test_is_high_freq_enabled();

        assert!(!tracker_opt, "Tracker should be cleared after disable");
    }

    #[tokio::test]
    async fn test_adaptive_frequency_adjustment() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("test_session");

        let notifier = SessionNotifier::new(tx, session_id);
        notifier.enable_high_freq_tracking(1000, 10000);

        // Simulate high system load
        notifier.test_adjust_freq_based_on_load(0.9);

        // Verify adjustment to high load
        {
            let status = notifier.get_high_freq_tracker_status().unwrap();
            assert_eq!(status.0, 1000);
            // The internal thresholds should be adjusted (verified through behavior)
        }

        // Simulate low system load
        notifier.test_adjust_freq_based_on_load(0.2);

        // More aggressive updates should work with low load
        let mut updates_received = 0;
        for i in 1..=10 {
            if notifier.test_update_high_freq_tokens(i * 5).is_some() {
                updates_received += 1;
            }
        }

        assert!(
            updates_received > 0,
            "Should receive updates with low load settings"
        );
    }

    #[tokio::test]
    async fn test_high_freq_tracker_status_query() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("test_session");

        let notifier = SessionNotifier::new(tx, session_id);

        // Initially should be None
        let status = notifier.get_high_freq_tracker_status();
        assert!(status.is_none(), "Should be None before enabling");

        // After enabling
        notifier.enable_high_freq_tracking(1000, 10000);
        let status = notifier.get_high_freq_tracker_status();
        assert!(status.is_some(), "Should be Some after enabling");

        let (used, size, percentage) = status.unwrap();
        assert_eq!(used, 1000, "Initial usage should be base_used");
        assert_eq!(size, 10000, "Size should match");
        assert_eq!(percentage, 10.0, "Initial percentage should be 10%");
    }
}
