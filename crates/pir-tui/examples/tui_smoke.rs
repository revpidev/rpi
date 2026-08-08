//! Minimal TUI smoke demo (T11 gate: real-terminal smoke).
//!
//! Boots `Tui` + `ProcessTerminal`, renders a `Text` and an animated
//! `Loader`, exits on `q` or after a 10s timeout, then stops the TUI (which
//! restores the terminal). Installs both recovery paths (panic hook +
//! SIGTERM/SIGHUP restore), so `kill -TERM <pid>` must also leave a restored
//! terminal and exit code 0. Run under a pty, e.g.:
//!
//! ```sh
//! script -qc 'cargo run -p pir-tui --example tui_smoke' /dev/null
//! ```

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pir_tui::components::loader::Loader;
use pir_tui::components::text::Text;
use pir_tui::terminal::ProcessTerminal;
use pir_tui::tui::{shared_component, InputListenerResult, Tui};

fn main() {
    let tui = Tui::new(Box::new(ProcessTerminal::new()));
    pir_tui::recovery::install_panic_hook(&tui);

    // Signal restore needs a running multi-threaded runtime (the returned
    // task restores the terminal and exits on SIGTERM/SIGHUP). Keep the
    // runtime entered for the rest of main: `Tui::start()` spawns the
    // SIGWINCH forwarder via the current runtime handle (terminal.rs
    // `spawn_resize_forwarder`), so without the guard resize events are
    // silently dropped.
    let runtime = tokio::runtime::Runtime::new();
    let _runtime_guard = runtime.as_ref().ok().map(|rt| rt.enter());
    let _signal_task = runtime
        .as_ref()
        .ok()
        .and_then(|_rt| pir_tui::recovery::spawn_signal_restore(&tui));

    tui.add_child(shared_component(Text::new(
        "pir-tui smoke demo — press q to quit (auto-exits after 10s)",
        1,
        1,
        None,
    )));
    let loader = Loader::new(
        tui.render_handle(),
        |frame| format!("\x1b[36m{frame}\x1b[0m"),
        std::string::ToString::to_string,
        "spinner running",
        None,
    );
    tui.add_child(shared_component(loader));

    let quit = Arc::new(AtomicBool::new(false));
    let quit_flag = Arc::clone(&quit);
    tui.add_input_listener(Box::new(move |data| {
        if data == "q" {
            quit_flag.store(true, Ordering::SeqCst);
            return Some(InputListenerResult {
                consume: true,
                data: None,
            });
        }
        None
    }));

    tui.start();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !quit.load(Ordering::SeqCst) && Instant::now() < deadline {
        let timeout = tui
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(100))
            .min(Duration::from_millis(100));
        tui.pump(Some(timeout));
    }

    tui.stop();
    let reason = if quit.load(Ordering::SeqCst) {
        "quit key"
    } else {
        "timeout"
    };
    // Past `stop()`: plain stdout, terminal already restored.
    let _ = writeln!(io::stdout(), "smoke ok ({reason})");
}
