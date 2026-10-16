//! Bandwidth Scheduling
//!
//! Provides time-based bandwidth limit scheduling. Allows setting different
//! download/upload limits for different times of day or days of the week.

use chrono::{Datelike, Local, Timelike};
use parking_lot::RwLock;

// Re-export protocol types for backward compatibility
pub use crate::protocol::{BandwidthLimits, ScheduleRule};

/// Bandwidth scheduler that manages time-based limits
pub struct BandwidthScheduler {
    /// Schedule rules (evaluated in order, first match wins)
    rules: Vec<ScheduleRule>,
    /// Default limits when no rule matches
    default_limits: BandwidthLimits,
    /// Current active limits (cached)
    current_limits: RwLock<BandwidthLimits>,
}

impl BandwidthScheduler {
    /// Create a new scheduler with the given rules and default limits
    pub fn new(rules: Vec<ScheduleRule>, default_limits: BandwidthLimits) -> Self {
        let current = Self::evaluate_rules(&rules, &default_limits);
        Self {
            rules,
            default_limits,
            current_limits: RwLock::new(current),
        }
    }

    /// Create a scheduler with no rules (always use default limits)
    pub fn with_defaults(download: Option<u64>, upload: Option<u64>) -> Self {
        Self::new(Vec::new(), BandwidthLimits { download, upload })
    }

    /// Get the current bandwidth limits
    pub fn get_limits(&self) -> BandwidthLimits {
        *self.current_limits.read()
    }

    /// Update limits based on current time
    ///
    /// This should be called periodically (e.g., every minute) to update limits
    /// when time-based rules are in effect.
    pub fn update(&self) -> bool {
        let new_limits = Self::evaluate_rules(&self.rules, &self.default_limits);
        let mut current = self.current_limits.write();
        if *current != new_limits {
            tracing::info!(
                "Bandwidth limits changed: download={:?} upload={:?}",
                new_limits.download,
                new_limits.upload
            );
            *current = new_limits;
            true
        } else {
            false
        }
    }

    /// Evaluate rules for the current time
    fn evaluate_rules(rules: &[ScheduleRule], default: &BandwidthLimits) -> BandwidthLimits {
        let now = Local::now();
        let hour = now.hour() as u8;
        let weekday = now.weekday();

        for rule in rules {
            if rule.matches(hour, weekday) {
                return BandwidthLimits {
                    download: rule.download_limit,
                    upload: rule.upload_limit,
                };
            }
        }

        *default
    }

    /// Get the rules
    pub fn rules(&self) -> &[ScheduleRule] {
        &self.rules
    }

    /// Add a new rule (appended to end of list)
    pub fn add_rule(&mut self, rule: ScheduleRule) {
        self.rules.push(rule);
        self.update();
    }

    /// Clear all rules
    pub fn clear_rules(&mut self) {
        self.rules.clear();
        self.update();
    }

    /// Set new rules (replaces existing)
    pub fn set_rules(&mut self, rules: Vec<ScheduleRule>) {
        self.rules = rules;
        self.update();
    }

    /// Set default limits
    pub fn set_defaults(&mut self, limits: BandwidthLimits) {
        self.default_limits = limits;
        self.update();
    }
}

impl Default for BandwidthScheduler {
    fn default() -> Self {
        Self::with_defaults(None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Weekday;

    #[test]
    fn test_schedule_rule_match_simple() {
        let rule = ScheduleRule::all_days(9, 17, Some(1_000_000), None);

        // During work hours
        assert!(rule.matches(9, Weekday::Mon));
        assert!(rule.matches(12, Weekday::Wed));
        assert!(rule.matches(17, Weekday::Fri));

        // Outside work hours
        assert!(!rule.matches(8, Weekday::Mon));
        assert!(!rule.matches(18, Weekday::Wed));
        assert!(!rule.matches(0, Weekday::Fri));
    }

    #[test]
    fn test_schedule_rule_match_overnight() {
        // Rule: 22:00 to 06:00 (overnight)
        let rule = ScheduleRule::all_days(22, 6, Some(10_000_000), None);

        // During overnight hours
        assert!(rule.matches(22, Weekday::Mon));
        assert!(rule.matches(23, Weekday::Mon));
        assert!(rule.matches(0, Weekday::Tue));
        assert!(rule.matches(3, Weekday::Tue));
        assert!(rule.matches(6, Weekday::Tue));

        // Outside overnight hours
        assert!(!rule.matches(7, Weekday::Mon));
        assert!(!rule.matches(12, Weekday::Mon));
        assert!(!rule.matches(21, Weekday::Mon));
    }

    #[test]
    fn test_schedule_rule_weekdays() {
        let rule = ScheduleRule::weekdays(9, 17, Some(500_000), None);

        // Weekdays
        assert!(rule.matches(12, Weekday::Mon));
        assert!(rule.matches(12, Weekday::Tue));
        assert!(rule.matches(12, Weekday::Wed));
        assert!(rule.matches(12, Weekday::Thu));
        assert!(rule.matches(12, Weekday::Fri));

        // Weekends
        assert!(!rule.matches(12, Weekday::Sat));
        assert!(!rule.matches(12, Weekday::Sun));
    }

    #[test]
    fn test_schedule_rule_weekends() {
        let rule = ScheduleRule::weekends(0, 23, None, None);

        // Weekends
        assert!(rule.matches(12, Weekday::Sat));
        assert!(rule.matches(12, Weekday::Sun));

        // Weekdays
        assert!(!rule.matches(12, Weekday::Mon));
        assert!(!rule.matches(12, Weekday::Fri));
    }

    #[test]
    fn test_scheduler_no_rules() {
        let scheduler = BandwidthScheduler::with_defaults(Some(1_000_000), Some(500_000));
        let limits = scheduler.get_limits();

        assert_eq!(limits.download, Some(1_000_000));
        assert_eq!(limits.upload, Some(500_000));
    }

    #[test]
    fn test_scheduler_first_match_wins() {
        // Rule 1: 0-23 all days, 1MB/s
        // Rule 2: 0-23 all days, 2MB/s
        // First rule should win
        let rules = vec![
            ScheduleRule::all_days(0, 23, Some(1_000_000), None),
            ScheduleRule::all_days(0, 23, Some(2_000_000), None),
        ];
        let scheduler = BandwidthScheduler::new(rules, BandwidthLimits::default());
        let limits = scheduler.get_limits();

        assert_eq!(limits.download, Some(1_000_000));
    }

    #[test]
    fn test_bandwidth_limits_default() {
        let limits = BandwidthLimits::default();
        assert_eq!(limits.download, None);
        assert_eq!(limits.upload, None);
    }
}
