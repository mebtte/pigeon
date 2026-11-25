use std::{
    io::{self, IsTerminal, Write},
    time::{Duration, Instant},
};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(350);
const BAR_WIDTH: usize = 20;

pub(super) struct UploadProgress {
    total: u64,
    sent: u64,
    last_sample: Instant,
    last_sample_bytes: u64,
    terminal: bool,
    line_width: usize,
}

impl UploadProgress {
    pub(super) fn new(total: u64) -> Self {
        let mut progress = Self {
            total,
            sent: 0,
            last_sample: Instant::now(),
            last_sample_bytes: 0,
            terminal: io::stderr().is_terminal(),
            line_width: 0,
        };
        progress.write_sample(0.0, total == 0);
        progress
    }

    pub(super) fn advance(&mut self, count: usize) {
        if count == 0 || self.sent == self.total {
            return;
        }
        self.sent = self.sent.saturating_add(count as u64).min(self.total);
        let now = Instant::now();
        let complete = self.sent == self.total;
        if complete || now.duration_since(self.last_sample) >= SAMPLE_INTERVAL {
            self.sample(now, complete);
        }
    }

    fn sample(&mut self, now: Instant, end_line: bool) {
        // Estimate throughput from file bytes handed off since the last sample.
        let rate = bytes_per_second(
            self.sent - self.last_sample_bytes,
            now.duration_since(self.last_sample),
        );
        self.write_sample(rate, end_line);
        self.last_sample = now;
        self.last_sample_bytes = self.sent;
    }

    fn write_sample(&mut self, rate: f64, end_line: bool) {
        let message = progress_message(self.sent, self.total, rate, self.terminal);
        let mut output = io::stderr().lock();
        // Progress output is best effort so a closed stderr does not interrupt the upload.
        if self.terminal {
            let pad = self.line_width.saturating_sub(message.len());
            let _ = write!(output, "\r{message}{:pad$}", "");
            self.line_width = message.len();
            if end_line {
                let _ = writeln!(output);
                self.line_width = 0;
            }
            let _ = output.flush();
        } else {
            let _ = writeln!(output, "{message}");
        }
    }
}

impl Drop for UploadProgress {
    fn drop(&mut self) {
        if self.sent != self.last_sample_bytes {
            self.sample(Instant::now(), true);
        } else if self.line_width > 0 {
            let _ = writeln!(io::stderr().lock());
        }
    }
}

fn bytes_per_second(bytes: u64, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        0.0
    } else {
        bytes as f64 / elapsed.as_secs_f64()
    }
}

fn progress_message(sent: u64, total: u64, rate: f64, terminal: bool) -> String {
    let percent = if total == 0 {
        100
    } else {
        u128::from(sent) * 100 / u128::from(total)
    };
    let sent_size = format_size(sent as f64);
    let total_size = format_size(total as f64);
    let rate = format_size(rate);
    let mut message = if terminal {
        let filled = percent as usize * BAR_WIDTH / 100;
        format!(
            "[{}{}] {percent:>3}%  {sent_size} / {total_size}  {rate}/s",
            "#".repeat(filled),
            "-".repeat(BAR_WIDTH - filled)
        )
    } else {
        format!("Upload progress: {percent}% ({sent_size} / {total_size}, {rate}/s)")
    };
    if sent == total {
        message.push_str(". Waiting for server confirmation...");
    }
    message
}

fn format_size(mut bytes: f64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024.0 {
        return format!("{bytes:.0} B");
    }
    let mut index = 0;
    while bytes >= 1024.0 && index + 1 < UNITS.len() {
        bytes /= 1024.0;
        index += 1;
    }
    format!("{bytes:.1} {}", UNITS[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_bytes_percent_and_recent_rate() {
        let rate = bytes_per_second(512, Duration::from_millis(500));
        assert_eq!(
            progress_message(512, 2048, rate, false),
            "Upload progress: 25% (512 B / 2.0 KiB, 1.0 KiB/s)"
        );
        assert_eq!(bytes_per_second(512, Duration::ZERO), 0.0);
    }

    #[test]
    fn terminal_draws_an_ascii_bar() {
        assert_eq!(
            progress_message(512, 2048, 1024.0, true),
            "[#####---------------]  25%  512 B / 2.0 KiB  1.0 KiB/s"
        );
        assert!(progress_message(0, 2048, 0.0, true).starts_with("[--------------------]"));
        assert!(progress_message(2048, 2048, 1024.0, true).starts_with("[####################]"));
    }

    #[test]
    fn waits_for_confirmation_only_after_all_file_bytes_are_sent() {
        let partial = progress_message(u64::MAX - 1, u64::MAX, 0.0, false);
        assert!(partial.starts_with("Upload progress: 99%"));
        assert!(!partial.contains("Waiting"));
        let empty = progress_message(0, 0, 0.0, false);
        assert_eq!(
            empty,
            "Upload progress: 100% (0 B / 0 B, 0 B/s). Waiting for server confirmation..."
        );
        let complete = progress_message(2048, 2048, 1024.0, false);
        assert!(complete.starts_with("Upload progress: 100%"));
        assert!(complete.ends_with("Waiting for server confirmation..."));
    }
}
