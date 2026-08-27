use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Instant;

use clap::ValueEnum;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ProgressMode {
    /// Interactive multi-line progress when stderr is a terminal, plain logs otherwise
    Auto,
    /// Always render interactive multi-line progress
    Color,
    /// Stable line-oriented progress, suitable for CI logs
    Plain,
    /// Suppress progress output; the final summary is still printed
    Quiet,
}

#[derive(Clone)]
pub struct Progress {
    inner: Arc<Inner>,
}

struct Inner {
    mode: ResolvedMode,
    bars: MultiProgress,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvedMode {
    Color,
    Plain,
    Quiet,
}

impl Progress {
    pub fn new(mode: ProgressMode) -> Self {
        let mode = match mode {
            ProgressMode::Auto if std::io::stderr().is_terminal() => ResolvedMode::Color,
            ProgressMode::Auto | ProgressMode::Plain => ResolvedMode::Plain,
            ProgressMode::Color => ResolvedMode::Color,
            ProgressMode::Quiet => ResolvedMode::Quiet,
        };
        let bars = MultiProgress::with_draw_target(match mode {
            ResolvedMode::Color => ProgressDrawTarget::stderr_with_hz(12),
            ResolvedMode::Plain | ResolvedMode::Quiet => ProgressDrawTarget::hidden(),
        });
        Self {
            inner: Arc::new(Inner { mode, bars }),
        }
    }

    pub fn spinner(&self, label: impl Into<String>) -> Task {
        self.task(label.into(), None)
    }

    pub fn bytes(&self, label: impl Into<String>, total: u64) -> Task {
        self.task(label.into(), Some(total))
    }

    fn task(&self, label: String, total: Option<u64>) -> Task {
        if self.inner.mode == ResolvedMode::Plain {
            eprintln!("⏵ {label}");
        }

        let bar = (self.inner.mode == ResolvedMode::Color).then(|| {
            let bar = self.inner.bars.add(match total {
                Some(total) => ProgressBar::new(total),
                None => ProgressBar::new_spinner(),
            });
            let style = match total {
                Some(_) => ProgressStyle::with_template(
                    "{spinner:.cyan} {msg:42} [{bar:24.cyan/blue}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12}",
                ),
                None => ProgressStyle::with_template(
                    "{spinner:.cyan} {msg:58} {elapsed_precise:>10}",
                ),
            }
            .expect("static progress template is valid")
            .progress_chars("=>-")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ");
            bar.set_style(style);
            bar.set_message(label.clone());
            bar.enable_steady_tick(std::time::Duration::from_millis(80));
            bar
        });

        Task {
            progress: self.clone(),
            label,
            started: Instant::now(),
            bar,
            finished: false,
        }
    }

    pub fn note(&self, message: impl AsRef<str>) {
        match self.inner.mode {
            ResolvedMode::Color => {
                let _ = self.inner.bars.println(format!("  · {}", message.as_ref()));
            }
            ResolvedMode::Plain => eprintln!("  · {}", message.as_ref()),
            ResolvedMode::Quiet => {}
        }
    }

    pub fn err_note(&self, message: impl AsRef<str>) {
        match self.inner.mode {
            ResolvedMode::Color => {
                let _ = self.inner.bars.println(format!("  ✗ {}", message.as_ref()));
            }
            ResolvedMode::Plain => eprintln!("  ✗ {}", message.as_ref()),
            ResolvedMode::Quiet => {}
        }
    }

    pub(crate) fn tracing_writer(&self) -> ProgressMakeWriter {
        ProgressMakeWriter {
            progress: self.clone(),
        }
    }

    pub(crate) fn tracing_ansi(&self) -> bool {
        self.inner.mode == ResolvedMode::Color && std::io::stderr().is_terminal()
    }

    fn write_diagnostic(&self, bytes: &[u8]) -> io::Result<()> {
        if self.inner.mode == ResolvedMode::Color {
            let message = String::from_utf8_lossy(bytes);
            let message = message.strip_suffix('\n').unwrap_or(message.as_ref());
            let message = message.strip_suffix('\r').unwrap_or(message);
            self.inner.bars.println(message)
        } else {
            let mut stderr = std::io::stderr().lock();
            stderr.write_all(bytes)?;
            stderr.flush()
        }
    }
}

pub struct Task {
    progress: Progress,
    label: String,
    started: Instant,
    bar: Option<ProgressBar>,
    finished: bool,
}

impl Task {
    pub fn progress_bar(&self) -> Option<ProgressBar> {
        self.bar.clone()
    }

    pub fn done(mut self) {
        self.finish(None);
    }

    pub fn done_with(mut self, detail: impl AsRef<str>) {
        self.finish(Some(detail.as_ref()));
    }

    fn finish(&mut self, detail: Option<&str>) {
        let elapsed = self.started.elapsed();
        let suffix = detail.map(|d| format!(" — {d}")).unwrap_or_default();
        match self.progress.inner.mode {
            ResolvedMode::Color => {
                self.clear_bar();
                let _ = self
                    .progress
                    .inner
                    .bars
                    .println(format!("✓ {}{suffix}", self.label));
            }
            ResolvedMode::Plain => {
                eprintln!("  ✓ {} ({}){}", self.label, format_elapsed(elapsed), suffix);
            }
            ResolvedMode::Quiet => {}
        }
        self.finished = true;
    }

    fn clear_bar(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
            self.progress.inner.bars.remove(&bar);
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        match self.progress.inner.mode {
            ResolvedMode::Color => {
                self.clear_bar();
                let _ = self
                    .progress
                    .inner
                    .bars
                    .println(format!("✗ {} — failed", self.label));
            }
            ResolvedMode::Plain => eprintln!(
                "  ✗ {} (failed after {})",
                self.label,
                format_elapsed(self.started.elapsed())
            ),
            ResolvedMode::Quiet => {}
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProgressMakeWriter {
    progress: Progress,
}

impl<'writer> MakeWriter<'writer> for ProgressMakeWriter {
    type Writer = ProgressEventWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        ProgressEventWriter {
            progress: self.progress.clone(),
        }
    }
}

pub(crate) struct ProgressEventWriter {
    progress: Progress,
}

impl Write for ProgressEventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.progress.write_diagnostic(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn format_elapsed(elapsed: std::time::Duration) -> String {
    if elapsed.as_secs_f64() >= 1.0 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}ms", elapsed.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use indicatif::TermLike;

    use super::*;

    #[derive(Clone, Debug, Default)]
    struct RecordingTerm {
        output: Arc<Mutex<String>>,
    }

    impl RecordingTerm {
        fn contents(&self) -> String {
            self.output.lock().unwrap().clone()
        }

        fn push(&self, value: &str) {
            self.output.lock().unwrap().push_str(value);
        }
    }

    impl TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            160
        }

        fn height(&self) -> u16 {
            40
        }

        fn move_cursor_up(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, value: &str) -> io::Result<()> {
            self.push(value);
            self.push("\n");
            Ok(())
        }

        fn write_str(&self, value: &str) -> io::Result<()> {
            self.push(value);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn tty_progress() -> (Progress, RecordingTerm) {
        let terminal = RecordingTerm::default();
        let bars = MultiProgress::with_draw_target(ProgressDrawTarget::term_like(Box::new(
            terminal.clone(),
        )));
        let progress = Progress {
            inner: Arc::new(Inner {
                mode: ResolvedMode::Color,
                bars,
            }),
        };
        (progress, terminal)
    }

    #[test]
    fn concurrent_task_completion_is_written_once() {
        let (progress, terminal) = tty_progress();
        let first = progress.spinner("first");
        let second = progress.spinner("second");

        second.done_with("complete");
        first.progress_bar().unwrap().tick();
        let third = progress.spinner("third");
        third.progress_bar().unwrap().tick();
        first.done();
        third.done();

        let output = terminal.contents();
        assert_eq!(
            output.matches("✓ second — complete").count(),
            1,
            "{output:?}"
        );
        assert_eq!(output.matches("✓ first").count(), 1, "{output:?}");
        assert_eq!(output.matches("✓ third").count(), 1, "{output:?}");
    }

    #[test]
    fn dropped_task_failure_is_written_once() {
        let (progress, terminal) = tty_progress();
        let active = progress.spinner("active");

        drop(progress.spinner("doomed"));
        active.progress_bar().unwrap().tick();
        active.done();

        let output = terminal.contents();
        assert_eq!(output.matches("✗ doomed — failed").count(), 1, "{output:?}");
    }

    #[test]
    fn diagnostic_is_coordinated_with_active_bars() {
        let (progress, terminal) = tty_progress();
        let task = progress.spinner("active");
        let mut writer = progress.tracing_writer().make_writer();

        writer.write_all(b"INFO diagnostic event\n").unwrap();
        task.progress_bar().unwrap().tick();
        task.done();

        let output = terminal.contents();
        assert_eq!(
            output.matches("INFO diagnostic event").count(),
            1,
            "{output:?}"
        );
        assert_eq!(output.matches("✓ active").count(), 1, "{output:?}");
    }
}
