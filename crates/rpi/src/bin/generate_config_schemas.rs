use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn write_schemas(
    directory: &Path,
    schemas: &std::collections::HashMap<&str, serde_json::Value>,
) -> Result<bool, String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let mut changed = false;
    for (name, schema) in schemas {
        let bytes = serde_json::to_vec_pretty(schema).map_err(|error| error.to_string())?;
        let mut bytes = bytes;
        bytes.push(b'\n');
        let path = directory.join(name);
        if !path.is_file() || std::fs::read(&path).map_err(|error| error.to_string())? != bytes {
            std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
            changed = true;
        }
    }
    Ok(changed)
}

fn check_schemas(
    directory: &Path,
    schemas: &std::collections::HashMap<&str, serde_json::Value>,
) -> Result<bool, String> {
    if !directory.is_dir() {
        return if directory.ends_with("rpi-pages/schemas") {
            println!(
                "note: {} not present; skipping site copy",
                directory.display()
            );
            Ok(true)
        } else {
            Err(format!(
                "missing repository schema directory: {directory:?}"
            ))
        };
    }
    let mut ok = true;
    for (name, schema) in schemas {
        let expected = format!(
            "{}\n",
            serde_json::to_string_pretty(schema).map_err(|error| error.to_string())?
        );
        let path = directory.join(name);
        if !path.is_file()
            || std::fs::read_to_string(&path).map_err(|error| error.to_string())? != expected
        {
            eprintln!("out of date: {}", path.display());
            ok = false;
        }
    }
    Ok(ok)
}

fn run() -> Result<bool, String> {
    let check = std::env::args().any(|argument| argument == "--check");
    let no_site = std::env::args().any(|argument| argument == "--no-site");
    let schemas = rpi::core::config_schemas::config_schemas();
    let repository = root().join("schemas");
    let site = root().join("../rpi-pages/schemas");
    if check {
        let repository_ok = check_schemas(&repository, &schemas)?;
        let site_ok = no_site || check_schemas(&site, &schemas)?;
        return Ok(repository_ok && site_ok);
    }
    let repository_changed = write_schemas(&repository, &schemas)?;
    let site_changed = no_site || write_schemas(&site, &schemas)?;
    println!(
        "config schemas generated from rpi typed models: {}{}",
        repository.display(),
        if no_site {
            String::new()
        } else {
            format!(", {}", site.display())
        }
    );
    Ok(repository_changed || site_changed)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}
