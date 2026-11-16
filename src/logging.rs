//! Dated, leveled lines on stderr. The systemd units append them to files that logrotate rotates.

use glurmo_model::UnixTime;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use tracing::Subscriber;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::clock::clock;

/// Starts logging to stderr at the levels `GLURMO_LOG` names, such as `debug` or `glurmo=debug,warn`,
/// and `info` without it. Panics are logged too. Records of the `log` crate, such as tokio-postgres's
/// server notices, come along.
pub fn start() {
    let asked = std::env::var("GLURMO_LOG")
        .ok()
        .map(|levels| levels.parse::<Targets>());
    let filter = match &asked {
        Some(Ok(targets)) => targets.clone(),
        _ => info(),
    };
    subscriber(std::io::stderr, filter).init();
    if let Some(Err(error)) = asked {
        tracing::warn!("GLURMO_LOG is not a list of levels, so glurmo logs at info: {error}");
    }
    log_panics();
}

/// `at` in the system's zone with its offset, as `2026-11-16T11:48:18-05:00`.
pub fn local(at: UnixTime) -> String {
    clock(at, &TimeZone::system(), "%Y-%m-%dT%H:%M:%S%:z")
}

fn info() -> Targets {
    Targets::new().with_default(tracing::Level::INFO)
}

/// Lines with a local time to the millisecond, a level and the module, written by `writer`. A line
/// that cannot be written is dropped, because reporting it would mean writing to stderr again.
fn subscriber<W>(writer: W, filter: Targets) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_timer(LocalTime)
            .with_ansi(false)
            .log_internal_errors(false),
    )
}

/// The time in the system's zone, with its offset, as `2026-11-16T11:48:18.123-05:00`.
struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let now = Timestamp::now();
        let offset = TimeZone::system().to_offset(now);
        write!(w, "{:.3}", now.display_with_offset(offset))
    }
}

/// Logs a panic, with a backtrace when `RUST_BACKTRACE` asks for one, in place of the default report.
/// A hook installed later, such as the TUI's, still runs this one after its own work.
fn log_panics() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        let message = info.payload_as_str().unwrap_or("no message");
        let at = info
            .location()
            .map_or_else(String::new, ToString::to_string);
        let backtrace = std::backtrace::Backtrace::capture();
        if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
            tracing::error!("thread {thread} panicked at {at}: {message}\n{backtrace}");
        } else {
            tracing::error!("thread {thread} panicked at {at}: {message}");
        }
    }));
}

/// Everything logged through [`capturing`], as text.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Captured {
    /// The lines logged so far.
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[cfg(test)]
impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A subscriber that logs as glurmo does, at `levels`, into the returned [`Captured`].
#[cfg(test)]
pub fn capturing(levels: &str) -> (impl Subscriber + Send + Sync, Captured) {
    let captured = Captured::default();
    let writer = captured.clone();
    (
        subscriber(move || writer.clone(), levels.parse().unwrap()),
        captured,
    )
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    /// A writer whose every write fails, as stderr does on a full disk.
    struct Full;

    impl io::Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(28))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(28))
        }
    }

    #[test]
    fn a_line_that_cannot_be_written_is_dropped() {
        let full = subscriber(|| Full, info());
        tracing::subscriber::with_default(full, || {
            tracing::error!("the disk is full");
        });
    }

    #[test]
    fn lines_carry_a_local_time_with_its_offset_and_a_level() {
        let (subscriber, captured) = capturing("info");
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("started");
            tracing::debug!("hidden");
        });
        let text = captured.text();
        let (time, rest) = text.split_once(' ').unwrap();
        let at: Timestamp = time
            .parse()
            .unwrap_or_else(|error| panic!("{time} is not a time with an offset: {error}"));
        assert!(at <= Timestamp::now());
        assert_eq!(
            time.split_once('.').unwrap().1.len(),
            "123-05:00".len(),
            "{time}"
        );
        assert!(
            rest.trim_start().starts_with("INFO glurmo::logging"),
            "{rest}"
        );
        assert!(!text.contains("hidden"), "{text}");
    }
}
