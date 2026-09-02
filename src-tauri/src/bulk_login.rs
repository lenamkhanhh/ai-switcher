use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::models::BulkLoginCodexInput;
use crate::store::set_owner_only_permissions;
use crate::tools::command_path;

pub(crate) const MAX_LOGIN_ACCOUNTS: usize = 100;
const MAX_CREDENTIALS_BYTES: usize = 5 * 1024 * 1024;
const MAX_WORKERS: u8 = 3;
const WORKER_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const BRIDGE_FILE: &str = "bulk_login_bridge.py";

/// Validate the user-owned credential file without returning or logging any secret material.
/// The credentials are only used to create a short-lived staging file for the external worker.
pub(crate) fn validate_credentials_file(raw: &str) -> Result<usize> {
    if raw.len() > MAX_CREDENTIALS_BYTES {
        bail!("Credential file is too large (maximum is 5 MB)");
    }

    let mut seen = HashSet::new();
    let mut count = 0usize;
    for (line_index, line) in raw.lines().enumerate() {
        let line_number = line_index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts = line.split('|').collect::<Vec<_>>();
        if !(2..=3).contains(&parts.len()) {
            bail!("Line {line_number} must use exactly email|password|2fa format");
        }
        let email = parts[0].trim();
        let password = parts[1].trim();
        let totp = parts.get(2).map_or("", |value| value.trim());
        if !looks_like_email(email) {
            bail!("Line {line_number} has an invalid email");
        }
        if password.is_empty() || password.len() > 4096 {
            bail!("Line {line_number} has a missing or oversized password");
        }
        if totp.len() > 512 || totp.contains(['\r', '\n']) {
            bail!("Line {line_number} has an invalid 2FA secret");
        }
        if !seen.insert(email.to_ascii_lowercase()) {
            bail!("Line {line_number} repeats a duplicate email");
        }
        count += 1;
        if count > MAX_LOGIN_ACCOUNTS {
            bail!("Credential file contains too many accounts (maximum is {MAX_LOGIN_ACCOUNTS})");
        }
    }
    if count == 0 {
        bail!("Credential file contains no accounts");
    }
    Ok(count)
}

fn looks_like_email(email: &str) -> bool {
    if email.len() > 320 || email.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && !domain.starts_with('.') && !domain.ends_with('.')
}

pub(crate) fn validate_worker_limit(workers: u8) -> Result<()> {
    if (1..=MAX_WORKERS).contains(&workers) {
        Ok(())
    } else {
        bail!("Worker count must be between 1 and {MAX_WORKERS}")
    }
}

pub(crate) fn validate_staging_acknowledgement(acknowledged: bool) -> Result<()> {
    if acknowledged {
        Ok(())
    } else {
        bail!("Confirm that you own these accounts before starting staging login")
    }
}

/// Build arguments for the bridge using paths only. Credential contents never become process
/// arguments, which keeps them out of command-line inspection and crash reports.
pub(crate) fn bridge_args(
    worker: &Path,
    accounts: &Path,
    output: &Path,
    workers: u8,
    headed: bool,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--worker"),
        worker.as_os_str().to_os_string(),
        OsString::from("--accounts"),
        accounts.as_os_str().to_os_string(),
        OsString::from("--output"),
        output.as_os_str().to_os_string(),
        OsString::from("--workers"),
        OsString::from(workers.to_string()),
    ];
    args.push(OsString::from(if headed {
        "--headed"
    } else {
        "--headless"
    }));
    args
}

/// Run the local Playwright worker and return its temporary OAuth export. The caller is expected
/// to pass the result straight to the existing OAuth batch importer and never persist it elsewhere.
pub(crate) fn run_staging_bulk_login(
    input: BulkLoginCodexInput,
    resource_dir: Option<&Path>,
) -> Result<String> {
    let BulkLoginCodexInput {
        credentials_text,
        worker_path,
        workers,
        headed,
        acknowledged,
    } = input;
    let credentials = zeroize::Zeroizing::new(credentials_text);
    validate_staging_acknowledgement(acknowledged)?;
    validate_credentials_file(&credentials)?;
    validate_worker_limit(workers)?;

    let worker = resolve_worker_script(worker_path.as_deref())?;
    let bridge = resolve_bridge_script(resource_dir)?;
    let (python, python_prefix) = resolve_python()?;

    let accounts_path = temporary_path("accounts");
    let output_path = temporary_path("oauth");
    if let Err(error) = write_secret_file(&accounts_path, credentials.as_bytes()) {
        secure_remove(&accounts_path);
        return Err(error);
    }
    if let Err(error) = create_protected_empty_file(&output_path) {
        secure_remove(&accounts_path);
        secure_remove(&output_path);
        return Err(error).context("Couldn't prepare the temporary OAuth export");
    }

    let result = run_worker_process(WorkerInvocation {
        python: &python,
        python_prefix: &python_prefix,
        bridge: &bridge,
        worker: &worker,
        accounts: &accounts_path,
        output: &output_path,
        workers,
        headed,
    });
    secure_remove(&accounts_path);

    let status = match result {
        Ok(status) => status,
        Err(error) => {
            secure_remove(&output_path);
            return Err(error);
        }
    };
    let output = match fs::read_to_string(&output_path) {
        Ok(output) => output,
        Err(error) => {
            secure_remove(&output_path);
            return Err(error).context("Bulk login worker did not produce an OAuth export");
        }
    };
    secure_remove(&output_path);
    if !status.success() {
        bail!("Bulk login worker failed (exit code {})", exit_code(status));
    }
    if output.len() > MAX_CREDENTIALS_BYTES {
        bail!("Bulk login worker returned an unexpectedly large OAuth export");
    }
    // Parse just enough to make a malformed worker result fail before it reaches state mutation.
    let value: serde_json::Value =
        serde_json::from_str(&output).context("Bulk login worker returned invalid JSON")?;
    if value
        .get("providerConnections")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|connections| connections.is_empty())
    {
        bail!("Bulk login worker returned no OAuth sessions");
    }
    Ok(output)
}

struct WorkerInvocation<'a> {
    python: &'a Path,
    python_prefix: &'a [String],
    bridge: &'a Path,
    worker: &'a Path,
    accounts: &'a Path,
    output: &'a Path,
    workers: u8,
    headed: bool,
}

fn run_worker_process(invocation: WorkerInvocation<'_>) -> Result<ExitStatus> {
    let args = bridge_args(
        invocation.worker,
        invocation.accounts,
        invocation.output,
        invocation.workers,
        invocation.headed,
    );
    let mut command = Command::new(invocation.python);
    command
        .args(invocation.python_prefix)
        .arg(invocation.bridge)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONUTF8", "1")
        .env("AI_SWITCHER_STAGING", "1");
    crate::tools::hide_command_window(&mut command);
    let mut child = command
        .spawn()
        .context("Couldn't start the staging login worker; check Python and Playwright")?;
    wait_for_worker(&mut child)
}

fn wait_for_worker(child: &mut Child) -> Result<ExitStatus> {
    let deadline = Instant::now() + WORKER_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Bulk login worker exceeded the 60-minute staging limit");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn resolve_python() -> Result<(PathBuf, Vec<String>)> {
    if let Some(path) = command_path("python") {
        return Ok((path, Vec::new()));
    }
    if let Some(path) = command_path("python3") {
        return Ok((path, Vec::new()));
    }
    if let Some(path) = command_path("py") {
        return Ok((path, vec!["-3".to_string()]));
    }
    bail!("Python 3 was not found on PATH");
}

fn resolve_worker_script(override_path: Option<&str>) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = override_path.map(str::trim).filter(|path| !path.is_empty()) {
        candidates.push(PathBuf::from(path));
    } else {
        if let Some(path) = std::env::var_os("AI_SWITCHER_9ROUTER_AUTO_LOGIN") {
            candidates.push(PathBuf::from(path));
        }
        candidates.push(PathBuf::from(
            r"D:\9router\Login tự động 9router v2\auto_login.py",
        ));
    }
    let candidate = candidates
        .into_iter()
        .find(|path| path.is_file())
        .context("9router auto_login.py was not found; choose its path in the staging dialog")?;
    let canonical = candidate
        .canonicalize()
        .context("Couldn't resolve the 9router auto_login.py path")?;
    if canonical.file_name().and_then(|name| name.to_str()) != Some("auto_login.py") {
        bail!("The worker path must point to auto_login.py");
    }
    Ok(canonical)
}

fn resolve_bridge_script(resource_dir: Option<&Path>) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(resource_dir) = resource_dir {
        candidates.push(resource_dir.join(BRIDGE_FILE));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join(BRIDGE_FILE),
    );
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .context("The staging login bridge is missing from this build")
}

fn resolve_temp_dir() -> PathBuf {
    std::env::temp_dir()
}

fn temporary_path(label: &str) -> PathBuf {
    resolve_temp_dir().join(format!("ai-switcher-{label}-{}.tmp", uuid::Uuid::new_v4()))
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("Couldn't create temporary {path:?} file"))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.flush()) {
        secure_remove(path);
        return Err(error.into());
    }
    if let Err(error) = set_owner_only_permissions(path) {
        secure_remove(path);
        return Err(error).context("Couldn't protect temporary credentials");
    }
    Ok(())
}

fn create_protected_empty_file(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("Couldn't create temporary {path:?} file"))?;
    drop(file);
    if let Err(error) = set_owner_only_permissions(path) {
        secure_remove(path);
        return Err(error);
    }
    Ok(())
}

fn secure_remove(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        if let Ok(mut file) = OpenOptions::new().write(true).open(path) {
            let mut remaining = metadata.len();
            let zeros = [0u8; 4096];
            while remaining > 0 {
                let chunk = remaining.min(zeros.len() as u64) as usize;
                if file.write_all(&zeros[..chunk]).is_err() {
                    break;
                }
                remaining -= chunk as u64;
            }
            let _ = file.flush();
        }
    }
    let _ = fs::remove_file(path);
}

fn exit_code(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "unknown".to_string(), |code| code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_bounded_email_password_totp_batch_without_returning_secrets() {
        let raw = "# local only\nfirst@example.com|secret-one|JBSWY3DPEHPK3PXP\nsecond@example.com|secret-two|\n";
        let count = validate_credentials_file(raw).expect("valid credentials file");
        assert_eq!(count, 2);
    }

    #[test]
    fn rejects_duplicate_email_without_echoing_password() {
        let raw = "first@example.com|super-secret|JBSWY3DPEHPK3PXP\nFIRST@example.com|another-secret|JBSWY3DPEHPK3PXP\n";
        let error = validate_credentials_file(raw).expect_err("duplicate must fail");
        let message = error.to_string();
        assert!(message.contains("duplicate"));
        assert!(!message.contains("super-secret"));
        assert!(!message.contains("another-secret"));
    }

    #[test]
    fn rejects_ambiguous_extra_fields() {
        let error = validate_credentials_file("user@example.com|password|totp|extra\n")
            .expect_err("extra field must fail");
        assert!(error.to_string().contains("exactly email|password|2fa"));
    }

    #[test]
    fn rejects_invalid_worker_limits() {
        assert!(validate_worker_limit(0).is_err());
        assert!(validate_worker_limit(4).is_err());
        assert!(validate_worker_limit(3).is_ok());
    }

    #[test]
    fn requires_explicit_account_ownership_acknowledgement() {
        assert!(validate_staging_acknowledgement(false).is_err());
        assert!(validate_staging_acknowledgement(true).is_ok());
    }

    #[test]
    fn bridge_args_never_include_credentials() {
        let args = bridge_args(
            std::path::Path::new("D:/9router/auto_login.py"),
            std::path::Path::new("C:/Temp/accounts.txt"),
            std::path::Path::new("C:/Temp/oauth.json"),
            2,
            true,
        );
        let rendered = args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|arg| arg == "--worker"));
        assert!(rendered.iter().any(|arg| arg == "--headed"));
        assert!(!rendered.iter().any(|arg| arg.contains("password")));
        assert!(!rendered.iter().any(|arg| arg.contains("totp")));
    }
}
