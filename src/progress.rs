//! Phase spinner. Hidden when stderr isn't a TTY so pipes stay clean.

use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

#[derive(Clone)]
pub struct Progress {
    bar: ProgressBar,
}

impl Progress {
    pub fn stderr() -> Self {
        let bar = if std::io::stderr().is_terminal() {
            let pb = ProgressBar::new_spinner();
            pb.set_draw_target(ProgressDrawTarget::stderr_with_hz(20));
            if let Ok(style) = ProgressStyle::with_template("{spinner:.cyan} {msg}") {
                pb.set_style(
                    style.tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                );
            }
            pb.enable_steady_tick(Duration::from_millis(50));
            pb
        } else {
            ProgressBar::hidden()
        };
        Self { bar }
    }

    pub fn silent() -> Self {
        Self {
            bar: ProgressBar::hidden(),
        }
    }

    pub fn phase(&self, msg: &str) {
        self.bar.set_message(msg.to_string());
        self.bar.tick();
    }

    pub fn done(&self) {
        self.bar.finish_and_clear();
    }
}
