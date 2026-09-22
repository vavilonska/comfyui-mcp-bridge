use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::process::Command;

use crate::{logging::redact_secrets, path_utils::strip_windows_verbatim_prefix};

/// Input accepted by the MCP download tool.
///
/// `model_id` and `version_id` are deliberately separate so callers never
/// depend on the Civitai CLI's ambiguous positional-id heuristic.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DownloadRequest {
    /// Download the default published version of this model.
    pub model_id: Option<u64>,
    /// Download this exact model version.
    pub version_id: Option<u64>,
    /// Select one file by file id, exact name, or unique name fragment.
    pub file: Option<String>,
    /// Download every file belonging to the selected version.
    #[serde(default)]
    pub all: bool,
    /// Explicit output directory. If omitted, the discovered ComfyUI root is
    /// used with the CLI's type-aware ComfyUI layout.
    pub target_dir: Option<PathBuf>,
    /// Re-download a file even when a valid target already exists.
    #[serde(default)]
    pub force: bool,
    /// Resolve and print the plan without downloading any bytes.
    #[serde(default)]
    pub dry_run: bool,
    /// Ask the CLI to warn about incompatible base-model families.
    pub for_base: Option<String>,
}

impl DownloadRequest {
    /// Validate relationships that JSON Schema cannot express portably.
    pub fn validate(&self) -> Result<()> {
        match (self.model_id, self.version_id) {
            (Some(1..), None) | (None, Some(1..)) => {}
            (Some(0), None) => bail!("model_id must be greater than zero"),
            (None, Some(0)) => bail!("version_id must be greater than zero"),
            (Some(_), Some(_)) => {
                bail!("exactly one of model_id or version_id must be supplied")
            }
            (None, None) => bail!("exactly one of model_id or version_id must be supplied"),
        }

        if self.file.is_some() && self.all {
            bail!("file and all are mutually exclusive");
        }
        if let Some(file) = &self.file {
            validate_text_value("file", file)?;
        }
        if let Some(for_base) = &self.for_base {
            validate_text_value("for_base", for_base)?;
        }
        if let Some(target) = &self.target_dir
            && target.as_os_str().is_empty()
        {
            bail!("target_dir cannot be empty");
        }
        Ok(())
    }

    /// Build the arguments following the executable name.
    ///
    /// An explicit `target_dir` is emitted as `--out-dir`. Otherwise the
    /// caller must supply the locally discovered ComfyUI root, which is
    /// emitted as the CLI's type-aware ComfyUI layout.
    pub fn build_download_args(&self, auto_comfy_root: Option<&Path>) -> Result<Vec<OsString>> {
        self.validate()?;

        let mut args = vec![
            OsString::from("--no-update-check"),
            OsString::from("download"),
        ];

        if let Some(model_id) = self.model_id {
            push_value(&mut args, "--model", model_id.to_string());
        } else if let Some(version_id) = self.version_id {
            push_value(&mut args, "--version", version_id.to_string());
        }

        if let Some(file) = &self.file {
            push_value(&mut args, "--file", file);
        } else if self.all {
            args.push(OsString::from("--all"));
        }

        if let Some(target_dir) = &self.target_dir {
            args.push(OsString::from("--out-dir"));
            args.push(target_dir.as_os_str().to_owned());
        } else {
            let root = auto_comfy_root.ok_or_else(|| {
                anyhow!("target_dir is required when a ComfyUI root was not discovered")
            })?;
            if !root.is_absolute() {
                bail!("the discovered ComfyUI root must be an absolute path");
            }
            args.extend([
                OsString::from("--layout"),
                OsString::from("comfyui"),
                OsString::from("--root"),
                root.as_os_str().to_owned(),
            ]);
        }

        if let Some(for_base) = &self.for_base {
            push_value(&mut args, "--for-base", for_base);
        }
        if self.force {
            args.push(OsString::from("--force"));
        }
        if self.dry_run {
            args.push(OsString::from("--dry-run"));
        }

        Ok(args)
    }

    /// Validate an explicit destination and then build download arguments.
    pub fn build_validated_download_args(
        &self,
        auto_comfy_root: Option<&Path>,
        approved_roots: &[PathBuf],
        allow_arbitrary: bool,
    ) -> Result<Vec<OsString>> {
        let mut validated = self.clone();
        if let Some(target) = &self.target_dir {
            validated.target_dir = Some(validate_download_target(
                target,
                auto_comfy_root,
                approved_roots,
                allow_arbitrary,
            )?);
        }
        validated.build_download_args(auto_comfy_root)
    }
}

fn validate_text_value(name: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{name} cannot be empty");
    }
    if value.chars().any(char::is_control) {
        bail!("{name} cannot contain control characters");
    }
    // A leading dash can be consumed as another flag by CLI flag parsers.
    if value.starts_with('-') {
        bail!("{name} cannot begin with '-'");
    }
    Ok(())
}

fn push_value(args: &mut Vec<OsString>, flag: &str, value: impl AsRef<OsStr>) {
    args.push(OsString::from(flag));
    args.push(value.as_ref().to_owned());
}

/// How the resolved CLI must be launched on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CliLauncher {
    Executable,
    Batch,
    PowerShell,
}

/// Information shown in the panel and exposed through MCP diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CliInfo {
    pub path: PathBuf,
    pub launcher: CliLauncher,
    pub version: String,
}

/// Successful CLI output. stderr is retained because the Civitai CLI writes
/// progress, warnings, and dry-run notes there.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CliOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct CliProcessError {
    pub exit_code: Option<i32>,
    pub stderr: String,
}

impl fmt::Display for CliProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self
            .exit_code
            .map_or_else(|| "terminated".to_owned(), |value| value.to_string());
        let redacted = redact_secrets(&self.stderr);
        let detail = redacted.trim();
        if detail.is_empty() {
            write!(
                formatter,
                "Civitai CLI failed ({code}): {}",
                explain_exit_code(self.exit_code)
            )
        } else {
            write!(
                formatter,
                "Civitai CLI failed ({code}, {}): {detail}",
                explain_exit_code(self.exit_code)
            )
        }
    }
}

impl Error for CliProcessError {}

/// Official Civitai CLI exit-code meanings. `None` means the process ended
/// without an exit code (for example, it was terminated by the OS).
pub fn explain_exit_code(code: Option<i32>) -> &'static str {
    match code {
        Some(0) => "success",
        Some(1) => "generic, filesystem, or validation error",
        Some(2) => "usage error or invalid argument",
        Some(3) => "not authorized; login or additional permission is required",
        Some(4) => "requested resource was not found",
        Some(5) => "network, transport, or service-unavailable failure; retry later",
        Some(6) => "rate limited; retry later",
        Some(_) => "unknown CLI error",
        None => "process terminated without an exit code",
    }
}

#[derive(Debug, Clone)]
pub struct CivitaiCli {
    path: PathBuf,
    launcher: CliLauncher,
}

impl CivitaiCli {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = canonical_cli_file(path.as_ref())?;
        let launcher = classify_launcher(&path).ok_or_else(|| {
            anyhow!(
                "unsupported Civitai CLI file type: {} (expected .exe, .cmd, .bat, .ps1, or an extensionless executable)",
                path.display()
            )
        })?;
        Ok(Self { path, launcher })
    }

    /// Resolve a configured file/directory first, otherwise search PATH.
    pub fn discover(configured_path: Option<&Path>) -> Result<Self> {
        let path = resolve_cli_path(configured_path)?;
        Self::from_path(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn launcher(&self) -> CliLauncher {
        self.launcher
    }

    /// Run `civitai version` to ensure the resolved launcher actually works.
    pub async fn probe(&self) -> Result<CliInfo> {
        let output = self
            .execute_checked(&[
                OsString::from("--no-update-check"),
                OsString::from("version"),
            ])
            .await
            .context("failed to probe the Civitai CLI version")?;
        let version = output.stdout.lines().next().unwrap_or_default().trim();
        if version.is_empty() {
            bail!("Civitai CLI version probe returned empty output");
        }
        Ok(CliInfo {
            path: self.path.clone(),
            launcher: self.launcher,
            version: version.to_owned(),
        })
    }

    /// Run a read-style CLI command and decode its `--json` stdout.
    ///
    /// Arguments are always passed as separate OS arguments. The method adds
    /// both the global update-check suppression and JSON flag.
    pub async fn run_json<T, I, S>(&self, args: I) -> Result<T>
    where
        T: DeserializeOwned,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cli_args = vec![OsString::from("--no-update-check")];
        cli_args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        cli_args.push(OsString::from("--json"));

        let output = self.execute_checked(&cli_args).await?;
        serde_json::from_str(&output.stdout).with_context(|| {
            format!(
                "Civitai CLI emitted invalid JSON: {}",
                output.stdout.chars().take(256).collect::<String>()
            )
        })
    }

    /// Execute a request after applying the explicit-directory allowlist.
    pub async fn download(
        &self,
        request: &DownloadRequest,
        auto_comfy_root: Option<&Path>,
        approved_roots: &[PathBuf],
        allow_arbitrary: bool,
    ) -> Result<CliOutput> {
        let args = request.build_validated_download_args(
            auto_comfy_root,
            approved_roots,
            allow_arbitrary,
        )?;
        self.execute_checked(&args).await
    }

    /// Execute already validated CLI arguments and capture output.
    pub async fn execute_checked(&self, args: &[OsString]) -> Result<CliOutput> {
        if self.launcher == CliLauncher::Batch {
            for value in args {
                let text = value.to_string_lossy();
                if text.contains(['&', '|', '<', '>', '^', '%', '!']) {
                    bail!(
                        "a .cmd/.bat Civitai wrapper cannot safely receive shell metacharacters; configure the .exe or .ps1 launcher instead"
                    );
                }
            }
        }
        let mut command = self.command(args);
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to start Civitai CLI at {}", self.path.display()))?;

        let stdout = redact_secrets(&String::from_utf8_lossy(&output.stdout));
        let stderr = redact_secrets(&String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            return Err(anyhow::Error::new(CliProcessError {
                exit_code: output.status.code(),
                stderr,
            }));
        }

        Ok(CliOutput {
            exit_code: output.status.code().unwrap_or(0),
            stdout,
            stderr,
        })
    }

    fn command(&self, args: &[OsString]) -> Command {
        let spec = self.command_spec(args);
        let mut command = Command::new(spec.program);
        command
            .args(spec.args)
            .env("CIVITAI_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    fn command_spec(&self, args: &[OsString]) -> CommandSpec {
        match self.launcher {
            CliLauncher::Executable => CommandSpec {
                program: self.path.as_os_str().to_owned(),
                args: args.to_vec(),
            },
            CliLauncher::Batch => {
                let mut shell_args = vec![
                    OsString::from("/D"),
                    OsString::from("/C"),
                    self.path.as_os_str().to_owned(),
                ];
                shell_args.extend_from_slice(args);
                CommandSpec {
                    program: windows_command_program(),
                    args: shell_args,
                }
            }
            CliLauncher::PowerShell => {
                let mut shell_args = vec![
                    OsString::from("-NoLogo"),
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-ExecutionPolicy"),
                    OsString::from("Bypass"),
                    OsString::from("-File"),
                    self.path.as_os_str().to_owned(),
                ];
                shell_args.extend_from_slice(args);
                CommandSpec {
                    program: powershell_program(),
                    args: shell_args,
                }
            }
        }
    }
}

#[derive(Debug)]
struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
}

/// Discover and probe the CLI in one call for state/panel initialization.
pub async fn probe_cli(configured_path: Option<&Path>) -> Result<CliInfo> {
    CivitaiCli::discover(configured_path)?.probe().await
}

/// Resolve a configured file/directory or search PATH for supported launchers.
pub fn resolve_cli_path(configured_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(configured) = configured_path {
        if configured.is_dir() {
            return find_in_directory(configured).ok_or_else(|| {
                anyhow!(
                    "no civitai executable or wrapper was found in configured directory {}",
                    configured.display()
                )
            });
        }
        if !configured.is_file() {
            bail!(
                "configured Civitai CLI path does not exist or is not a file: {}",
                configured.display()
            );
        }
        return canonical_cli_file(configured);
    }

    let path = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&path) {
        if let Some(candidate) = find_in_directory(&directory) {
            return Ok(candidate);
        }
    }
    bail!("Civitai CLI was not found; configure civitai_cli_path or add civitai to PATH")
}

fn find_in_directory(directory: &Path) -> Option<PathBuf> {
    const NAMES: &[&str] = &[
        "civitai.exe",
        "civitai.ps1",
        "civitai.cmd",
        "civitai.bat",
        "civitai",
    ];
    NAMES
        .iter()
        .map(|name| directory.join(name))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| fs::canonicalize(candidate).ok())
}

fn powershell_program() -> OsString {
    find_program_on_path("pwsh.exe")
        .or_else(|| {
            env::var_os("SystemRoot")
                .map(PathBuf::from)
                .map(|root| {
                    root.join("System32")
                        .join("WindowsPowerShell")
                        .join("v1.0")
                        .join("powershell.exe")
                })
                .filter(|path| path.is_file())
        })
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
        .into_os_string()
}

fn windows_command_program() -> OsString {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| root.join("System32").join("cmd.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("cmd.exe"))
        .into_os_string()
}

fn find_program_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn canonical_cli_file(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("Civitai CLI path is not a file: {}", path.display());
    }
    fs::canonicalize(path)
        .map(strip_windows_verbatim_prefix)
        .with_context(|| format!("failed to resolve Civitai CLI path {}", path.display()))
}

fn classify_launcher(path: &Path) -> Option<CliLauncher> {
    match path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("exe") => Some(CliLauncher::Executable),
        Some("cmd" | "bat") => Some(CliLauncher::Batch),
        Some("ps1") => Some(CliLauncher::PowerShell),
        None => Some(CliLauncher::Executable),
        _ => None,
    }
}

/// Validate and normalize a remotely supplied explicit output directory.
///
/// By default it must be inside the discovered ComfyUI root or an approved
/// root. `allow_arbitrary` removes that allowlist requirement, but does not
/// permit UNC/device paths, filesystem roots, or obvious system directories.
pub fn validate_download_target(
    target: &Path,
    comfyui_root: Option<&Path>,
    approved_roots: &[PathBuf],
    allow_arbitrary: bool,
) -> Result<PathBuf> {
    if target.as_os_str().is_empty() {
        bail!("target_dir cannot be empty");
    }
    if !target.is_absolute() {
        bail!("target_dir must be an absolute local path");
    }
    if is_unc_or_device_path(target) {
        bail!("UNC and Windows device paths are not allowed for target_dir");
    }
    if target
        .components()
        .any(|component| component == Component::ParentDir)
    {
        bail!("target_dir cannot contain '..' components");
    }
    if target.is_file() {
        bail!("target_dir points to a file, not a directory");
    }

    let resolved = resolve_with_existing_ancestor(target)?;
    if is_obvious_system_path(&resolved) {
        bail!(
            "target_dir is a filesystem root or protected system directory: {}",
            resolved.display()
        );
    }
    if allow_arbitrary {
        return Ok(resolved);
    }

    let mut roots = Vec::with_capacity(approved_roots.len() + 1);
    if let Some(root) = comfyui_root {
        roots.push(root.to_path_buf());
    }
    roots.extend_from_slice(approved_roots);

    for root in roots {
        if !root.is_absolute() || is_unc_or_device_path(&root) {
            continue;
        }
        if let Ok(root) = resolve_with_existing_ancestor(&root)
            && path_starts_with(&resolved, &root)
        {
            return Ok(resolved);
        }
    }

    bail!("target_dir is outside the discovered ComfyUI root and approved download roots")
}

fn is_unc_or_device_path(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy();
    text.starts_with(r"\\") || text.starts_with("//")
}

/// Canonicalize the longest existing ancestor, then append missing components.
/// This gives containment checks real symlink semantics without requiring the
/// final download directory to exist yet.
fn resolve_with_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut cursor = path;
    let mut missing = Vec::new();

    while !cursor.exists() {
        let name = cursor.file_name().ok_or_else(|| {
            anyhow!(
                "target path has no resolvable existing ancestor: {}",
                path.display()
            )
        })?;
        missing.push(name.to_owned());
        cursor = cursor.parent().ok_or_else(|| {
            anyhow!(
                "target path has no resolvable existing ancestor: {}",
                path.display()
            )
        })?;
    }

    let metadata = fs::metadata(cursor)
        .with_context(|| format!("failed to inspect target ancestor {}", cursor.display()))?;
    if !metadata.is_dir() {
        bail!(
            "target path has a non-directory ancestor: {}",
            cursor.display()
        );
    }

    let mut resolved = fs::canonicalize(cursor)
        .map(strip_windows_verbatim_prefix)
        .with_context(|| format!("failed to resolve target ancestor {}", cursor.display()))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let mut path_components = path.components();
    for root_component in root.components() {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        if !component_eq(path_component, root_component) {
            return false;
        }
    }
    true
}

fn component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    if cfg!(windows) {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    } else {
        left == right
    }
}

fn is_obvious_system_path(path: &Path) -> bool {
    if !path
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return true;
    }

    #[cfg(windows)]
    {
        let system_roots = [
            env::var_os("SystemRoot"),
            env::var_os("WINDIR"),
            env::var_os("ProgramFiles"),
            env::var_os("ProgramFiles(x86)"),
            env::var_os("ProgramData"),
        ];
        for root in system_roots.into_iter().flatten() {
            let root = PathBuf::from(root);
            let resolved_root = resolve_with_existing_ancestor(&root).unwrap_or(root);
            if path_starts_with(path, &resolved_root) {
                return true;
            }
        }
    }

    #[cfg(not(windows))]
    {
        const SYSTEM_ROOTS: &[&str] = &[
            "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/sbin", "/sys", "/usr",
        ];
        if SYSTEM_ROOTS
            .iter()
            .any(|root| path_starts_with(path, Path::new(root)))
        {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DownloadRequest {
        DownloadRequest {
            model_id: None,
            version_id: Some(691_639),
            file: None,
            all: false,
            target_dir: None,
            force: false,
            dry_run: false,
            for_base: None,
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn request_requires_exactly_one_identifier() {
        let mut value = request();
        value.version_id = None;
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );

        value.model_id = Some(42);
        value.version_id = Some(43);
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
    }

    #[test]
    fn file_and_all_are_mutually_exclusive() {
        let mut value = request();
        value.file = Some("vae".to_owned());
        value.all = true;
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("mutually exclusive")
        );
    }

    #[test]
    fn automatic_download_uses_comfyui_layout_and_exact_version() {
        let mut value = request();
        value.all = true;
        value.force = true;
        value.dry_run = true;
        value.for_base = Some("SDXL 1.0".to_owned());
        let root = if cfg!(windows) {
            Path::new(r"C:\ComfyUI")
        } else {
            Path::new("/opt/ComfyUI")
        };
        let args = strings(&value.build_download_args(Some(root)).unwrap());
        assert_eq!(
            args,
            vec![
                "--no-update-check",
                "download",
                "--version",
                "691639",
                "--all",
                "--layout",
                "comfyui",
                "--root",
                root.to_string_lossy().as_ref(),
                "--for-base",
                "SDXL 1.0",
                "--force",
                "--dry-run",
            ]
        );
    }

    #[test]
    fn explicit_download_uses_out_dir_and_model_flag() {
        let target = if cfg!(windows) {
            PathBuf::from(r"D:\Models")
        } else {
            PathBuf::from("/data/models")
        };
        let mut value = request();
        value.version_id = None;
        value.model_id = Some(4_384);
        value.file = Some("vae".to_owned());
        value.target_dir = Some(target.clone());
        let args = strings(&value.build_download_args(None).unwrap());
        assert_eq!(
            args,
            vec![
                "--no-update-check",
                "download",
                "--model",
                "4384",
                "--file",
                "vae",
                "--out-dir",
                target.to_string_lossy().as_ref(),
            ]
        );
    }

    #[test]
    fn configured_directory_resolves_supported_wrapper() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("civitai.cmd");
        fs::write(&candidate, "@echo off\r\n").unwrap();
        let resolved = resolve_cli_path(Some(temp.path())).unwrap();
        assert_eq!(resolved, fs::canonicalize(candidate).unwrap());
    }

    #[test]
    fn launch_specs_keep_every_value_as_a_distinct_argument() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("civitai.cmd");
        fs::write(&wrapper, "@echo off\r\n").unwrap();
        let cli = CivitaiCli::from_path(&wrapper).unwrap();
        let remote_value = OsString::from("name with spaces & punctuation");
        let spec = cli.command_spec(&[OsString::from("--file"), remote_value.clone()]);
        assert!(spec.program.to_string_lossy().ends_with("cmd.exe"));
        assert_eq!(spec.args[0..2], ["/D", "/C"].map(OsString::from));
        assert_eq!(
            spec.args[2],
            strip_windows_verbatim_prefix(fs::canonicalize(wrapper).unwrap())
        );
        assert_eq!(spec.args[3], OsString::from("--file"));
        assert_eq!(spec.args[4], remote_value);
    }

    #[test]
    fn allowlist_accepts_descendant_and_rejects_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let comfy = temp.path().join("ComfyUI");
        let sibling = temp.path().join("elsewhere");
        fs::create_dir_all(&comfy).unwrap();
        fs::create_dir_all(&sibling).unwrap();

        let accepted = comfy.join("models").join("custom");
        let resolved = validate_download_target(&accepted, Some(&comfy), &[], false).unwrap();
        assert!(path_starts_with(
            &resolved,
            &strip_windows_verbatim_prefix(fs::canonicalize(&comfy).unwrap())
        ));

        let rejected = validate_download_target(&sibling, Some(&comfy), &[], false);
        assert!(rejected.unwrap_err().to_string().contains("outside"));
    }

    #[test]
    fn approved_root_is_honored_and_arbitrary_mode_is_still_local() {
        let temp = tempfile::tempdir().unwrap();
        let approved = temp.path().join("approved");
        let arbitrary = temp.path().join("arbitrary");
        fs::create_dir_all(&approved).unwrap();
        fs::create_dir_all(&arbitrary).unwrap();

        assert!(
            validate_download_target(
                &approved.join("loras"),
                None,
                std::slice::from_ref(&approved),
                false,
            )
            .is_ok()
        );
        assert!(validate_download_target(&arbitrary, None, &[], true).is_ok());
        assert!(
            validate_download_target(Path::new(r"\\server\share\models"), None, &[], true).is_err()
        );
    }

    #[test]
    fn exit_codes_have_actionable_meanings() {
        assert!(explain_exit_code(Some(3)).contains("authorized"));
        assert!(explain_exit_code(Some(5)).contains("retry"));
        assert!(explain_exit_code(Some(6)).contains("rate limited"));
        assert!(explain_exit_code(None).contains("terminated"));
    }
}
