//! `GOZ_TIMING`: where one run of goz spends its time, printed on stderr
//! when the variable is set, so a search's cost can be split between
//! starting the process, reaching gozd, gozd's answer and the output.

use std::time::{Duration, Instant};

/// The moments of one run, counted from the start of `main`.
pub(crate) struct Timing {
    /// How long Windows took to start the process before `main` ran.
    launch: Option<Duration>,
    start: Instant,
    connected: Option<Duration>,
    first_page: Option<Duration>,
    last_page: Option<Duration>,
    pages: u32,
    rows: u64,
}

impl Timing {
    /// Timing for a run whose `main` started at `start`, `launch` after
    /// Windows created the process.
    pub(crate) fn new(launch: Option<Duration>, start: Instant) -> Self {
        Self {
            launch,
            start,
            connected: None,
            first_page: None,
            last_page: None,
            pages: 0,
            rows: 0,
        }
    }

    /// The pipe to gozd is open and trusted.
    pub(crate) fn connected(&mut self) {
        self.connected_at(self.start.elapsed());
    }

    /// A page of `rows` rows has arrived and been handed to the output.
    pub(crate) fn page(&mut self, rows: usize) {
        self.page_at(self.start.elapsed(), rows);
    }

    fn connected_at(&mut self, at: Duration) {
        self.connected = Some(at);
    }

    fn page_at(&mut self, at: Duration, rows: usize) {
        self.first_page.get_or_insert(at);
        self.last_page = Some(at);
        self.pages += 1;
        self.rows += rows as u64;
    }

    /// The report line for a run that is done now.
    pub(crate) fn report(&self) -> String {
        self.report_at(self.start.elapsed())
    }

    fn report_at(&self, done: Duration) -> String {
        let ms = |d: Duration| format!("{:.1} ms", d.as_secs_f64() * 1000.0);
        let mut parts = Vec::new();
        if let Some(launch) = self.launch {
            parts.push(format!("process start {}", ms(launch)));
        }
        let marks = [
            ("connected", self.connected),
            ("first page", self.first_page),
            ("last page", self.last_page),
            ("done", Some(done)),
        ];
        for (what, at) in marks {
            if let Some(at) = at {
                parts.push(format!("{what} at {}", ms(at)));
            }
        }
        let pages = match self.pages {
            1 => "1 page".to_owned(),
            n => format!("{n} pages"),
        };
        format!(
            "goz: timing: {}; {pages}, {} rows\n",
            parts.join(", "),
            self.rows
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Timing;
    use std::time::{Duration, Instant};

    fn ms(n: f64) -> Duration {
        Duration::from_secs_f64(n / 1000.0)
    }

    /// Process start comes first, then each moment from the start of
    /// `main`, then what came back.
    #[test]
    fn a_search_reports_every_moment() {
        let mut timing = Timing::new(Some(ms(21.44)), Instant::now());
        timing.connected_at(ms(2.3));
        timing.page_at(ms(61.0), 4096);
        timing.page_at(ms(70.24), 100);
        assert_eq!(
            timing.report_at(ms(71.0)),
            "goz: timing: process start 21.4 ms, connected at 2.3 ms, first page at 61.0 ms, \
             last page at 70.2 ms, done at 71.0 ms; 2 pages, 4196 rows\n"
        );
    }

    /// A run that never reached gozd reports only what it knows.
    #[test]
    fn a_run_without_a_daemon_reports_what_it_knows() {
        let timing = Timing::new(None, Instant::now());
        assert_eq!(
            timing.report_at(ms(3.0)),
            "goz: timing: done at 3.0 ms; 0 pages, 0 rows\n"
        );
    }
}
