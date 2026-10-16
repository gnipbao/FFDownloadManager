//! Bandwidth scheduling types
//!
//! Types for time-based bandwidth limit scheduling.

use chrono::Weekday;
use serde::{Deserialize, Serialize};

/// A rule for time-based bandwidth scheduling
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleRule {
    /// Start hour (0-23, inclusive)
    pub start_hour: u8,
    /// End hour (0-23, inclusive). If end < start, rule wraps around midnight
    pub end_hour: u8,
    /// Days of week this rule applies (empty = all days)
    #[serde(default)]
    pub days: Vec<Weekday>,
    /// Download speed limit in bytes/sec (None = unlimited)
    pub download_limit: Option<u64>,
    /// Upload speed limit in bytes/sec (None = unlimited)
    pub upload_limit: Option<u64>,
}

impl ScheduleRule {
    /// Create a new schedule rule
    pub fn new(
        start_hour: u8,
        end_hour: u8,
        days: Vec<Weekday>,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) -> Self {
        Self {
            start_hour: start_hour.min(23),
            end_hour: end_hour.min(23),
            days,
            download_limit,
            upload_limit,
        }
    }

    /// Create a rule that applies to all days
    pub fn all_days(
        start_hour: u8,
        end_hour: u8,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) -> Self {
        Self::new(
            start_hour,
            end_hour,
            Vec::new(),
            download_limit,
            upload_limit,
        )
    }

    /// Create a rule for weekdays only (Mon-Fri)
    pub fn weekdays(
        start_hour: u8,
        end_hour: u8,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) -> Self {
        Self::new(
            start_hour,
            end_hour,
            vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ],
            download_limit,
            upload_limit,
        )
    }

    /// Create a rule for weekends only (Sat-Sun)
    pub fn weekends(
        start_hour: u8,
        end_hour: u8,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) -> Self {
        Self::new(
            start_hour,
            end_hour,
            vec![Weekday::Sat, Weekday::Sun],
            download_limit,
            upload_limit,
        )
    }

    /// Check if this rule matches the given time
    pub fn matches(&self, hour: u8, weekday: Weekday) -> bool {
        // Check day of week
        if !self.days.is_empty() && !self.days.contains(&weekday) {
            return false;
        }

        // Check time range
        if self.start_hour <= self.end_hour {
            // Normal range (e.g., 9-17)
            hour >= self.start_hour && hour <= self.end_hour
        } else {
            // Wraps around midnight (e.g., 22-6)
            hour >= self.start_hour || hour <= self.end_hour
        }
    }
}

/// Current bandwidth limits (possibly from schedule)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BandwidthLimits {
    /// Download speed limit in bytes/sec (None = unlimited)
    pub download: Option<u64>,
    /// Upload speed limit in bytes/sec (None = unlimited)
    pub upload: Option<u64>,
}
