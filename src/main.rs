use anyhow::{bail, Context, Result};
use blake3::Hasher;
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Parser)]
#[command(
    name = "cmdshim",
    version,
    about = "Project-local command shims for mise"
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Materialize shims and print the directory that should be added to PATH.
    Path {
        /// Explicit mise.toml path. Otherwise searches upward from the current directory.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Execute a configured shim command.
    Exec {
        /// Explicit mise.toml path. Generated shims always pass this.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Name under [_.cmdshim.<name>].
        name: String,
        /// Arguments appended to the configured command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, Deserialize)]
struct RootConfig {
    #[serde(default)]
    cmdshim: BTreeMap<String, ShimConfig>,
    #[serde(rename = "_", default)]
    private: PrivateConfig,
}

#[derive(Debug, Default, Deserialize)]
struct PrivateConfig {
    #[serde(default)]
    cmdshim: BTreeMap<String, ShimConfig>,
}

#[derive(Debug)]
struct Config {
    cmdshim: BTreeMap<String, ShimConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct ShimConfig {
    run: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn main() {
    let code = match real_main() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cmdshim: {err:#}");
            1
        }
    };
    std::process::exit(code);
}

fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Path { config } => {
            let config_path = resolve_config(config.as_deref())?;
            let text = fs::read_to_string(&config_path)
                .with_context(|| format!("failed to read {}", config_path.display()))?;
            let parsed = parse_config(&text, &config_path)?;
            let out = materialize(&config_path, &text, &parsed)?;
            println!("{}", out.display());
            Ok(0)
        }
        CliCommand::Exec { config, name, args } => {
            let config_path = resolve_config(config.as_deref())?;
            let text = fs::read_to_string(&config_path)
                .with_context(|| format!("failed to read {}", config_path.display()))?;
            let parsed = parse_config(&text, &config_path)?;
            exec_shim(&config_path, &parsed, &name, &args)
        }
    }
}

fn resolve_config(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return canonical_file(path);
    }

    let mut dir = env::current_dir().context("failed to determine current directory")?;
    loop {
        for candidate in ["mise.toml", ".mise.toml"] {
            let path = dir.join(candidate);
            if path.is_file() {
                return canonical_file(&path);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    bail!("no mise.toml or .mise.toml found in this directory or any parent directory")
}

fn canonical_file(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))
}

fn parse_config(text: &str, path: &Path) -> Result<Config> {
    let root: RootConfig =
        toml::from_str(text).with_context(|| format!("failed to parse {}", path.display()))?;

    let mut shims = root.cmdshim;
    for (name, shim) in root.private.cmdshim {
        if shims.insert(name.clone(), shim).is_some() {
            bail!("shim {name:?} is defined in both [cmdshim.{name}] and [_.cmdshim.{name}]");
        }
    }

    if shims.is_empty() {
        bail!("{} contains no [_.cmdshim.<name>] entries", path.display());
    }
    for (name, shim) in &shims {
        validate_name(name)?;
        if shim.run.is_empty() {
            bail!("[_.cmdshim.{name}].run must contain at least one argument")
        }
    }
    Ok(Config { cmdshim: shims })
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name == "cmdshim"
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        bail!("invalid shim name {name:?}")
    }
    Ok(())
}

fn materialize(config_path: &Path, config_text: &str, cfg: &Config) -> Result<PathBuf> {
    let root = cache_root()?;
    let current_exe = env::current_exe().context("failed to determine cmdshim executable path")?;
    let current_exe = fs::canonicalize(&current_exe).unwrap_or(current_exe);
    let project_key = short_hash(config_path.to_string_lossy().as_bytes());
    let dir = root.join(project_key);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create cache directory {}", dir.display()))?;

    let mut state_hasher = Hasher::new();
    state_hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    state_hasher.update(b"\0");
    state_hasher.update(config_text.as_bytes());
    state_hasher.update(b"\0");
    state_hasher.update(current_exe.to_string_lossy().as_bytes());
    let state = state_hasher.finalize().to_hex().to_string();
    let state_path = dir.join(".cmdshim-state");
    let current = fs::read_to_string(&state_path).ok();

    if current.as_deref() != Some(&state) || !all_shims_exist(&dir, cfg) {
        regenerate(&dir, config_path, &current_exe, cfg)?;
        atomic_write(&state_path, state.as_bytes())?;
    }

    Ok(dir)
}

fn all_shims_exist(dir: &Path, cfg: &Config) -> bool {
    cfg.cmdshim
        .keys()
        .all(|name| dir.join(name).is_file() && dir.join(format!("{name}.cmd")).is_file())
}

fn regenerate(dir: &Path, config_path: &Path, cmdshim_exe: &Path, cfg: &Config) -> Result<()> {
    // This hashed directory is private to cmdshim; replace its generated files in place.
    if dir.is_dir() {
        for entry in
            fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.file_name().and_then(|x| x.to_str()) == Some(".cmdshim-state") {
                continue;
            }
            if path.is_file() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove stale shim {}", path.display()))?;
            }
        }
    }

    for name in cfg.cmdshim.keys() {
        write_posix_shim(&dir.join(name), cmdshim_exe, config_path, name)?;
        write_cmd_shim(
            &dir.join(format!("{name}.cmd")),
            cmdshim_exe,
            config_path,
            name,
        )?;
    }
    Ok(())
}

fn write_posix_shim(path: &Path, cmdshim_exe: &Path, config_path: &Path, name: &str) -> Result<()> {
    let body = format!(
        "#!/usr/bin/env sh\nexec {} exec --config {} {} -- \"$@\"\n",
        sh_quote(&cmdshim_exe.to_string_lossy()),
        sh_quote(&config_path.to_string_lossy()),
        sh_quote(name),
    );
    atomic_write(path, body.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn write_cmd_shim(path: &Path, cmdshim_exe: &Path, config_path: &Path, name: &str) -> Result<()> {
    let exe = cmd_escape_quoted(&cmdshim_exe.to_string_lossy());
    let config = cmd_escape_quoted(&config_path.to_string_lossy());
    let name = cmd_escape_quoted(name);
    let body = format!(
        "@echo off\r\nsetlocal DisableDelayedExpansion\r\n\"{exe}\" exec --config \"{config}\" \"{name}\" -- %*\r\nexit /b %ERRORLEVEL%\r\n"
    );
    atomic_write(path, body.as_bytes())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension()
            .and_then(|x| x.to_str())
            .unwrap_or("cmdshim"),
        std::process::id()
    ));
    fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed to replace {}", path.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to move {} to {}", tmp.display(), path.display()))
}

fn exec_shim(config_path: &Path, cfg: &Config, name: &str, args: &[OsString]) -> Result<i32> {
    let shim = cfg
        .cmdshim
        .get(name)
        .with_context(|| format!("no [_.cmdshim.{name}] entry in {}", config_path.display()))?;
    if shim.run.is_empty() {
        bail!("[_.cmdshim.{name}].run must not be empty")
    }

    let config_root = config_path
        .parent()
        .context("configuration file has no parent directory")?;
    let expanded: Vec<String> = shim.run.iter().map(|s| expand(s, config_root)).collect();

    let cwd = match shim.cwd.as_deref() {
        Some(value) => {
            let path = PathBuf::from(expand(value, config_root));
            if path.is_absolute() {
                path
            } else {
                config_root.join(path)
            }
        }
        None => config_root.to_path_buf(),
    };

    let mut cmd = Command::new(&expanded[0]);
    cmd.args(&expanded[1..]);
    cmd.args(args);
    cmd.current_dir(&cwd);

    for (key, value) in &shim.env {
        cmd.env(key, expand(value, config_root));
    }

    let status = cmd.status().with_context(|| {
        format!(
            "failed to execute {:?} for [_.cmdshim.{name}] (cwd: {})",
            expanded[0],
            cwd.display()
        )
    })?;

    if let Some(code) = status.code() {
        return Ok(code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Ok(128 + signal);
        }
    }

    eprintln!("cmdshim: child terminated without an exit code; returning 1");
    Ok(1)
}

fn expand(input: &str, config_root: &Path) -> String {
    input.replace("{{config_root}}", &config_root.to_string_lossy())
}

fn cache_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CMDSHIM_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        if let Some(path) = env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(path).join("cmdshim"));
        }
    }

    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("cmdshim"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache").join("cmdshim"));
    }
    bail!("cannot determine cache directory; set CMDSHIM_CACHE_DIR")
}

fn short_hash(bytes: &[u8]) -> String {
    let hex = blake3::hash(bytes).to_hex().to_string();
    hex[..16].to_owned()
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn cmd_escape_quoted(value: &str) -> String {
    // Quotes inside quoted cmd.exe arguments are represented by doubled quotes.
    // Percent signs must be doubled to avoid environment-variable expansion.
    value.replace('%', "%%").replace('"', "\"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_config_root() {
        let p = Path::new("/tmp/project");
        assert_eq!(
            expand("{{config_root}}/Cargo.toml", p),
            "/tmp/project/Cargo.toml"
        );
    }

    #[test]
    fn shell_quote_handles_apostrophe() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn rejects_path_names() {
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name("foo\\bar").is_err());
        assert!(validate_name("cmdshim").is_err());
        assert!(validate_name("okay").is_ok());
        assert!(validate_name("code-gen.v2").is_ok());
    }

    #[test]
    fn parses_mise_private_namespace() {
        let cfg = parse_config(
            r#"
                [_.cmdshim.acme]
                run = ["cargo", "run", "--"]
            "#,
            Path::new("mise.toml"),
        )
        .unwrap();
        assert_eq!(cfg.cmdshim["acme"].run[0], "cargo");
    }

    #[test]
    fn accepts_legacy_top_level_namespace() {
        let cfg = parse_config(
            r#"
                [cmdshim.acme]
                run = ["echo"]
            "#,
            Path::new("mise.toml"),
        )
        .unwrap();
        assert!(cfg.cmdshim.contains_key("acme"));
    }

    #[test]
    fn rejects_duplicate_namespaces() {
        let err = parse_config(
            r#"
                [cmdshim.acme]
                run = ["echo"]
                [_.cmdshim.acme]
                run = ["printf"]
            "#,
            Path::new("mise.toml"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("defined in both"));
    }
}
