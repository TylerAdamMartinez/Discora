use std::{
    io,
    path::Path,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

const REFRESH: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageUnits {
    #[default]
    Decimal,
    Binary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageUsage {
    pub used: u64,
    pub total: u64,
}

impl StorageUsage {
    pub fn from_total_free(total: u64, free: u64) -> io::Result<Self> {
        if total == 0 || free > total {
            return Err(io::Error::other("Invalid filesystem capacity"));
        }
        Ok(Self {
            used: total - free,
            total,
        })
    }

    fn label(self, compact: bool, units: StorageUnits) -> String {
        let scales = match units {
            StorageUnits::Decimal => [
                ("TB", 1_000_000_000_000_u64),
                ("GB", 1_000_000_000),
                ("MB", 1_000_000),
                ("KB", 1_000),
            ],
            StorageUnits::Binary => [
                ("TiB", 1_099_511_627_776_u64),
                ("GiB", 1_073_741_824),
                ("MiB", 1_048_576),
                ("KiB", 1_024),
            ],
        };
        let (unit, divisor) = scales
            .into_iter()
            .find(|(_, divisor)| self.total >= *divisor)
            .unwrap_or(("B", 1));
        let divisor = divisor as f64;
        let number = |bytes| {
            format!("{:.2}", bytes as f64 / divisor)
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        };
        let used = number(self.used);
        let total = number(self.total);
        let percent = self.used as f64 / self.total as f64 * 100.0;
        if compact {
            format!("{used} / {total} {unit} ({percent:.2}%)")
        } else {
            format!("Used {used} {unit} out of {total} {unit} ({percent:.2}%)")
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
enum Status {
    #[default]
    Checking,
    Available(StorageUsage),
    Unavailable,
}

#[derive(Default)]
pub struct Monitor {
    pub units: StorageUnits,
    status: Status,
    pending: Option<(Instant, Receiver<io::Result<StorageUsage>>)>,
    next_refresh: Option<Instant>,
}

impl Monitor {
    #[cfg(test)]
    pub fn with_usage(used: u64, total: u64) -> Self {
        Self {
            status: Status::Available(StorageUsage { used, total }),
            ..Self::default()
        }
    }

    pub fn poll(&mut self, path: &Path, now: Instant) {
        if let Some((started, receiver)) = &self.pending {
            let expired = now.duration_since(*started) >= TIMEOUT;
            match receiver.try_recv() {
                Ok(result) => {
                    self.status = if expired {
                        Status::Unavailable
                    } else {
                        result.map(Status::Available).unwrap_or(Status::Unavailable)
                    };
                    self.pending = None;
                    self.next_refresh = Some(now + REFRESH);
                }
                Err(TryRecvError::Disconnected) => {
                    self.status = Status::Unavailable;
                    self.pending = None;
                    self.next_refresh = Some(now + REFRESH);
                }
                Err(TryRecvError::Empty) if expired => self.status = Status::Unavailable,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.pending.is_none() && self.next_refresh.is_none_or(|due| now >= due) {
            let path = path.to_owned();
            let (sender, receiver) = mpsc::channel();
            // A network filesystem query may block in the OS. Never join this
            // worker from the UI or teardown, and never start overlapping queries.
            thread::spawn(move || {
                let _ = sender.send(crate::platform::storage_usage(&path));
            });
            self.pending = Some((now, receiver));
        }
    }

    pub fn label(&self, width: u16) -> Option<String> {
        let full = match self.status {
            Status::Checking => "Storage: checking…".to_owned(),
            Status::Unavailable => "Storage: unavailable".to_owned(),
            Status::Available(usage) => usage.label(false, self.units),
        };
        if full.chars().count() <= usize::from(width) {
            return Some(full);
        }
        if let Status::Available(usage) = self.status {
            let compact = usage.label(true, self.units);
            if compact.len() <= usize::from(width) {
                return Some(compact);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_families_use_correct_divisors_and_keep_percentages() {
        for (total, decimal, binary) in [
            (1, "B", "B"),
            (1_024, "KB", "KiB"),
            (1_048_576, "MB", "MiB"),
            (1_073_741_824, "GB", "GiB"),
            (1_099_511_627_776, "TB", "TiB"),
        ] {
            let usage = StorageUsage { used: total, total };
            for (units, label) in [
                (StorageUnits::Decimal, decimal),
                (StorageUnits::Binary, binary),
            ] {
                let text = usage.label(false, units);
                assert!(text.contains(label), "{text}");
                assert!(text.ends_with("(100.00%)"));
            }
        }
        let mut monitor = Monitor::with_usage(1_000_000_000_000, 1_000_000_000_000);
        assert_eq!(
            monitor.label(60).unwrap(),
            "Used 1 TB out of 1 TB (100.00%)"
        );
        monitor.units = StorageUnits::Binary;
        assert_eq!(
            monitor.label(60).unwrap(),
            "Used 931.32 GiB out of 931.32 GiB (100.00%)"
        );
        let usage = StorageUsage {
            used: 1_000_000_000_000,
            total: 2_000_000_000_000,
        };
        assert_eq!(
            usage.label(true, StorageUnits::Binary),
            "0.91 / 1.82 TiB (50.00%)"
        );
        assert_eq!(
            usage.label(true, StorageUnits::Decimal),
            "1 / 2 TB (50.00%)"
        );
    }

    #[test]
    fn formats_binary_units_and_rounds_percentages() {
        let mut monitor = Monitor::with_usage(2 * 1_099_511_627_776, 12 * 1_099_511_627_776);
        monitor.units = StorageUnits::Binary;
        assert_eq!(
            monitor.label(60).unwrap(),
            "Used 2 TiB out of 12 TiB (16.67%)"
        );
        assert_eq!(monitor.label(25).unwrap(), "2 / 12 TiB (16.67%)");
        assert_eq!(monitor.label(10), None);
        assert_eq!(
            Monitor {
                units: StorageUnits::Binary,
                ..Monitor::with_usage(0, 500 * 1_073_741_824)
            }
            .label(60)
            .unwrap(),
            "Used 0 GiB out of 500 GiB (0.00%)"
        );
        assert_eq!(
            Monitor {
                units: StorageUnits::Binary,
                ..Monitor::with_usage(5 * 1_073_741_824 / 4, 5 * 1_073_741_824 / 4)
            }
            .label(60)
            .unwrap(),
            "Used 1.25 GiB out of 1.25 GiB (100.00%)"
        );
        assert!(StorageUsage::from_total_free(0, 0).is_err());
        assert!(StorageUsage::from_total_free(10, 11).is_err());
    }

    #[test]
    fn refreshes_off_thread_and_reports_failure_without_substituting_a_drive() {
        let directory = tempfile::tempdir().unwrap();
        let mut monitor = Monitor::default();
        assert_eq!(monitor.label(60).unwrap(), "Storage: checking…");
        let start = Instant::now();
        monitor.poll(directory.path(), start);
        let deadline = start + Duration::from_secs(5);
        while monitor.pending.is_some() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
            monitor.poll(directory.path(), Instant::now());
        }
        assert!(matches!(monitor.status, Status::Available(_)));
        let due = monitor.next_refresh.unwrap();
        let missing = directory.path().join("missing");
        monitor.poll(&missing, due - Duration::from_millis(1));
        assert!(monitor.pending.is_none());
        monitor.poll(&missing, due);
        while monitor.pending.is_some() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
            monitor.poll(&missing, due + Duration::from_millis(20));
        }
        assert_eq!(monitor.label(60).unwrap(), "Storage: unavailable");
    }

    #[test]
    fn timed_out_query_never_overlaps_or_restores_a_late_result() {
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        let mut monitor = Monitor {
            pending: Some((start, receiver)),
            ..Monitor::with_usage(1, 2)
        };
        let later = start + TIMEOUT;
        monitor.poll(Path::new("unused"), later);
        assert_eq!(monitor.status, Status::Unavailable);
        assert!(monitor.pending.is_some());
        sender.send(StorageUsage::from_total_free(10, 5)).unwrap();
        monitor.poll(Path::new("unused"), later);
        assert_eq!(monitor.status, Status::Unavailable);
        assert!(monitor.pending.is_none());
        assert_eq!(monitor.next_refresh, Some(later + REFRESH));
    }
}
