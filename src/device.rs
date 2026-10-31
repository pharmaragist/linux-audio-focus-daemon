//! Output device tracking.
//!
//! PipeWire has no stable D-Bus API for the default sink, so the daemon asks
//! `pactl` at a low rate. A change is reported when the default sink appears,
//! disappears, or moves to another device.

use std::collections::BTreeSet;
use std::process::Command;
use std::time::{Duration, Instant};

/// A change in the default output device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A device became the default sink (headphones plugged in, or the
    /// default moved to another device that is still present).
    Attached,
    /// The default sink disappeared (headphones unplugged).
    Detached,
}

/// Polls the default output sink and reports device changes.
pub struct DeviceMonitor {
    interval: Duration,
    next: Instant,
    enabled: bool,
    default: Option<String>,
}

impl DeviceMonitor {
    /// `poll_ms` is the check interval. Zero disables tracking entirely.
    pub fn new(poll_ms: u64) -> Self {
        Self {
            interval: Duration::from_millis(poll_ms.max(1)),
            next: Instant::now(),
            enabled: poll_ms > 0,
            default: None,
        }
    }

    /// True when the next poll is due.
    pub fn due(&self, now: Instant) -> bool {
        self.enabled && now >= self.next
    }

    /// Check for a device change. Returns `None` when nothing changed, when
    /// the check is not due yet, or when `pactl` is unavailable.
    pub fn poll(&mut self, now: Instant) -> Option<Event> {
        if !self.enabled || now < self.next {
            return None;
        }
        self.next = now + self.interval;

        let Some(default) = default_sink() else {
            self.enabled = false;
            eprintln!("lafd: output device tracking disabled: pactl is not available");
            return None;
        };
        let Some(sinks) = list_sinks() else {
            self.default = Some(default);
            return None;
        };

        let event = classify(self.default.as_deref(), &default, &sinks);
        self.default = Some(default);
        event
    }

    /// The default sink seen by the last successful poll.
    pub fn sink(&self) -> Option<&str> {
        self.default.as_deref().filter(|sink| !sink.is_empty())
    }
}

/// Compare two snapshots of the default sink. The sink set tells whether the
/// previous default disappeared or the default simply moved.
fn classify(previous: Option<&str>, current: &str, sinks: &BTreeSet<String>) -> Option<Event> {
    let previous = previous?;
    if previous == current {
        return None;
    }
    if current.is_empty() {
        return Some(Event::Detached);
    }
    if previous.is_empty() {
        return Some(Event::Attached);
    }
    if !sinks.contains(previous) {
        return Some(Event::Detached);
    }
    Some(Event::Attached)
}

/// `pactl get-default-sink`. `None` means the command could not be started;
/// an empty string means the server has no default sink right now.
fn default_sink() -> Option<String> {
    let output = Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `pactl list short sinks`, second column is the sink name.
fn list_sinks() -> Option<BTreeSet<String>> {
    let output = Command::new("pactl")
        .arg("list")
        .arg("short")
        .arg("sinks")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .map(str::to_string)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn first_poll_is_silent() {
        let sinks = set(&["speaker"]);
        assert_eq!(classify(None, "speaker", &sinks), None);
    }

    #[test]
    fn unchanged_default_is_silent() {
        let sinks = set(&["speaker"]);
        assert_eq!(classify(Some("speaker"), "speaker", &sinks), None);
    }

    #[test]
    fn new_sink_is_attached() {
        let sinks = set(&["speaker", "headphones"]);
        assert_eq!(
            classify(Some("speaker"), "headphones", &sinks),
            Some(Event::Attached)
        );
    }

    #[test]
    fn missing_old_default_is_detached() {
        let sinks = set(&["speaker"]);
        assert_eq!(
            classify(Some("headphones"), "speaker", &sinks),
            Some(Event::Detached)
        );
    }

    #[test]
    fn no_default_sink_is_detached() {
        assert_eq!(
            classify(Some("headphones"), "", &BTreeSet::new()),
            Some(Event::Detached)
        );
    }

    #[test]
    fn device_back_after_silence_is_attached() {
        let sinks = set(&["headphones"]);
        assert_eq!(
            classify(Some(""), "headphones", &sinks),
            Some(Event::Attached)
        );
    }

    #[test]
    fn move_between_present_sinks_is_attached() {
        let sinks = set(&["speaker", "headphones"]);
        assert_eq!(
            classify(Some("speaker"), "headphones", &sinks),
            Some(Event::Attached)
        );
    }
}
