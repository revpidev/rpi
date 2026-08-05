//! Induced-panic terminal restore check (T11 §8.5 self-test, subprocess level).
//!
//! Boots `Tui` + `ProcessTerminal` with the recovery panic hook installed,
//! renders one frame, then panics on purpose. Expected: the terminal restore
//! sequence (bracketed paste off, Kitty protocol pop, cursor show, raw mode
//! off) is written BEFORE the panic message reaches stderr, so the invoking
//! shell is left sane. Run under a pty, e.g.:
//!
//! ```sh
//! script -qc 'cargo run -p pir-tui --example tui_panic_restore' /dev/null
//! ```

use std::time::Instant;

use pir_tui::components::text::Text;
use pir_tui::terminal::ProcessTerminal;
use pir_tui::tui::{shared_component, Tui};

fn main() {
    let tui = Tui::new(Box::new(ProcessTerminal::new()));
    pir_tui::recovery::install_panic_hook(&tui);

    tui.add_child(shared_component(Text::new("about to panic", 1, 1, None)));
    tui.start();
    // Drive one render so there is on-screen content to restore under.
    tui.tick(Instant::now());

    panic!("intentional panic: verify terminal restore (T11 coding-standards §8.5)");
}
