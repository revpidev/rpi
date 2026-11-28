#![cfg(all(target_os = "linux", target_env = "gnu"))]

#[test]
fn production_entries_initialize_before_argument_parsing() {
    use std::process::Command;

    for binary in [env!("CARGO_BIN_EXE_rpi"), env!("CARGO_BIN_EXE_rpi-rpc")] {
        let output = Command::new(binary)
            .arg("--version")
            .env("MALLOC_ARENA_MAX", "0")
            .output()
            .expect("production entry process");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "binary={binary}, stderr={stderr}");
        assert!(
            stderr.contains("invalid MALLOC_ARENA_MAX"),
            "binary={binary}, stderr={stderr}"
        );
    }
}

#[cfg(feature = "v19-benchmark")]
#[test]
fn arena_early_entry_matrix() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    let cases: Vec<(Option<OsString>, usize)> = vec![
        (None, 1),
        (Some(OsString::from("0")), 1),
        (Some(OsString::from("-1")), 1),
        (Some(OsString::from("abc")), 1),
        (Some(OsString::from("2x")), 1),
        (Some(OsString::from("")), 1),
        (Some(OsString::from("999999999999999999999")), 1),
        (Some(OsString::from_vec(vec![0xff])), 1),
        (Some(OsString::from("2")), 2),
        (Some(OsString::from("4")), 4),
        (Some(OsString::from("+2")), 2),
    ];
    for (value, expected) in cases {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rpi-v19-replay"));
        command.env("RPI_V19_ARENA_PROBE", "1");
        if let Some(value) = &value {
            command.env("MALLOC_ARENA_MAX", value);
        } else {
            command.env_remove("MALLOC_ARENA_MAX");
        }
        let output = command.output().expect("arena probe process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "value={value:?}, expected={expected}, stdout={stdout}, stderr={stderr}"
        );
        assert_eq!(
            stdout.trim().parse::<usize>().expect("heap count"),
            expected,
            "value={value:?}, stderr={stderr}"
        );
        let invalid = value.as_ref().is_some_and(|value| {
            value
                .to_str()
                .is_none_or(|value| !matches!(value.parse::<libc::c_int>(), Ok(limit) if limit > 0))
        });
        assert_eq!(
            stderr.contains("invalid MALLOC_ARENA_MAX"),
            invalid,
            "value={value:?}, stderr={stderr}"
        );
    }
}
