//! `normalize-diff` — manual parity check CLI (fixtures/README.md §2/§4).
//!
//! Normalizes both inputs with fresh `rpi-test-support` normalizers and diffs
//! them. JSONL inputs keep line order; non-JSONL input falls back to text
//! normalization. Exit code 0 = equal after normalization, 1 = difference
//! (report on stderr), 2 = usage/IO error.
//!
//! Usage:
//!   cargo run -p rpi-test-support --example normalize-diff -- <expected> <actual> [path-to-strip...]

use std::process::ExitCode;

use rpi_test_support::normalize::Normalizer;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: normalize-diff <expected> <actual> [path-to-strip...]");
        return ExitCode::from(2);
    }
    let read = |p: &str| -> Result<String, String> {
        std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))
    };
    let (expected, actual) = match (read(&args[0]), read(&args[1])) {
        (Ok(e), Ok(a)) => (e, a),
        (Err(err), _) | (_, Err(err)) => {
            eprintln!("{err}");
            return ExitCode::from(2);
        }
    };

    let normalize = |text: &str| -> String {
        let mut n = Normalizer::new();
        for p in &args[2..] {
            n = n.with_path(p.clone());
        }
        n.normalize_jsonl(text)
            .unwrap_or_else(|_| n_fall_back(text, &args[2..]))
    };
    fn n_fall_back(text: &str, paths: &[String]) -> String {
        let mut n = Normalizer::new();
        for p in paths {
            n = n.with_path(p.clone());
        }
        n.normalize_string(text)
    }

    let (e, a) = (normalize(&expected), normalize(&actual));
    match rpi_test_support::diff::diff_jsonl(&e, &a) {
        Ok(()) => {
            println!("OK: inputs are equal after normalization");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::FAILURE
        }
    }
}
