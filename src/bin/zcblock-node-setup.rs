use reqwest::Url;
use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};
use std::env;
use std::error::Error;
use std::ffi::{CStr, CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;
use zcutils::kernel_module_artifacts::inspect_kernel_module;

type AnyError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, AnyError>;

const HOST_ROOT: &str = "/host";
const MODULE_NAME: &str = "zcnblk_client_mod";
const MODULE_FILE: &str = "zcnblk_client_mod.ko";

fn main() {
    let mode = env::args().nth(1).unwrap_or_else(|| "setup".to_string());
    let result = match mode.as_str() {
        "setup" => run_setup(),
        "cache" => run_cache(),
        "-h" | "--help" | "help" => {
            println!(
                "Usage: zcblock-node-setup [setup|cache]\n\
                 setup  select, verify, and load the zcnblk client-edge module\n\
                 cache  refresh a verified HTTP module artifact in the host cache"
            );
            Ok(())
        }
        _ => Err(format!("unknown mode {mode:?}; expected setup or cache").into()),
    };
    if let Err(error) = result {
        eprintln!("zcblock-node-setup: ERROR: {error}");
        std::process::exit(1);
    }
}

#[derive(Clone, Debug)]
struct NodeIdentity {
    kernel: String,
    architecture: String,
}

fn node_identity() -> Result<NodeIdentity> {
    let mut value = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: `uname` initializes the supplied `utsname` on success.
    if unsafe { libc::uname(value.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: the successful `uname` call above initialized `value`.
    let value = unsafe { value.assume_init() };
    let field = |bytes: &[libc::c_char]| -> Result<String> {
        // SAFETY: every `utsname` field returned by `uname` is NUL terminated.
        let text = unsafe { CStr::from_ptr(bytes.as_ptr()) }
            .to_str()
            .map_err(|error| format!("uname returned non-UTF-8 data: {error}"))?;
        validate_identity(text)?;
        Ok(text.to_string())
    };
    Ok(NodeIdentity {
        kernel: field(&value.release)?,
        architecture: field(&value.machine)?,
    })
}

fn validate_identity(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
    {
        return Err(format!("unsafe kernel or architecture identity: {value:?}").into());
    }
    Ok(())
}

fn env_value(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    match env::var(name) {
        Ok(value) if value == "true" => Ok(true),
        Ok(value) if value == "false" => Ok(false),
        Ok(value) => Err(format!("{name} must be true or false, got {value:?}").into()),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn env_positive_u64(name: &str, default: u64) -> Result<u64> {
    match env::var(name) {
        Ok(value) => {
            let parsed = value
                .parse::<u64>()
                .map_err(|_| format!("{name} must be a positive integer, got {value:?}"))?;
            if parsed == 0 {
                return Err(format!("{name} must be greater than zero").into());
            }
            Ok(parsed)
        }
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn expand_template(value: &str, identity: &NodeIdentity) -> String {
    value
        .replace("%KERNEL_RELEASE%", &identity.kernel)
        .replace("%ARCH%", &identity.architecture)
}

fn validate_absolute_path(label: &str, path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(format!("{label} must be an absolute path: {}", path.display()).into());
    }
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(format!(
                    "{label} must not contain '.' or '..' components: {}",
                    path.display()
                )
                .into());
            }
        }
    }
    Ok(())
}

fn host_path(path: &Path) -> Result<PathBuf> {
    validate_absolute_path("host path", path)?;
    Ok(Path::new(HOST_ROOT).join(path.strip_prefix("/")?))
}

fn validate_sha256(label: &str, value: &str) -> Result<String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{label} must contain exactly 64 hexadecimal characters").into());
    }
    Ok(value.to_ascii_lowercase())
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = file_sha256(path)?;
    if actual != expected {
        return Err(format!(
            "module SHA-256 mismatch: expected={expected} actual={actual} path={}",
            path.display()
        )
        .into());
    }
    Ok(())
}

struct RemoveOnDrop(PathBuf);

impl RemoveOnDrop {
    fn disarm(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

fn unique_partial(destination: &Path, suffix: &str) -> PathBuf {
    let mut value = destination.as_os_str().to_os_string();
    value.push(format!(".{suffix}.{}", std::process::id()));
    PathBuf::from(value)
}

fn set_readable_mode(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn atomic_copy(source: &Path, destination: &Path, expected: Option<&str>) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("destination has no parent: {}", destination.display()))?;
    fs::create_dir_all(parent)?;
    let partial = unique_partial(destination, "partial");
    let cleanup = RemoveOnDrop(partial.clone());
    fs::copy(source, &partial)?;
    set_readable_mode(&partial)?;
    if let Some(expected) = expected {
        verify_sha256(&partial, expected)?;
    }
    fs::rename(&partial, destination)?;
    cleanup.disarm();
    Ok(())
}

fn configure_host_command(command: &mut Command) {
    // SAFETY: this closure runs in the forked child before exec. It changes only
    // that child's root and current directory, and reports OS errors to Command.
    unsafe {
        command.pre_exec(|| {
            let root = b"/host\0";
            let slash = b"/\0";
            if libc::chroot(root.as_ptr().cast()) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::chdir(slash.as_ptr().cast()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn host_output(program: &str, args: &[OsString]) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(args);
    configure_host_command(&mut command);
    Ok(command.output()?)
}

fn host_success(program: &str, args: &[OsString]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    configure_host_command(&mut command);
    let status = command.status()?;
    if !status.success() {
        return Err(format!("host command {program} failed with {status}").into());
    }
    Ok(())
}

fn host_command(name: &str) -> Option<String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    let args = vec![
        OsString::from("-c"),
        OsString::from("command -v \"$ZCBLOCK_HOST_COMMAND\""),
    ];
    let mut command = Command::new("/bin/sh");
    command.args(args).env("ZCBLOCK_HOST_COMMAND", name);
    configure_host_command(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if !value.starts_with('/') || value.contains(char::is_whitespace) {
        return None;
    }
    Some(value.to_string())
}

fn package_manager() -> Option<(&'static str, String)> {
    for name in ["dnf", "yum", "apt-get", "zypper", "apk"] {
        if let Some(path) = host_command(name) {
            return Some((name, path));
        }
    }
    None
}

fn os_args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn install_module_tools() -> Result<()> {
    let Some((kind, path)) = package_manager() else {
        return Err("insmod/modinfo are absent and the host package manager is unsupported".into());
    };
    match kind {
        "dnf" | "yum" => host_success(&path, &os_args(&["install", "-y", "kmod"])),
        "apt-get" => {
            host_success(&path, &os_args(&["update"]))?;
            host_success(&path, &os_args(&["install", "-y", "kmod"]))
        }
        "zypper" => host_success(&path, &os_args(&["--non-interactive", "install", "kmod"])),
        "apk" => host_success(&path, &os_args(&["add", "kmod"])),
        _ => unreachable!(),
    }
}

fn install_build_dependencies(kernel: &str, need_headers: bool) -> Result<()> {
    let Some((kind, path)) = package_manager() else {
        return Err(
            "development build dependencies are absent and the host package manager is unsupported"
                .into(),
        );
    };
    let mut args: Vec<String> = Vec::new();
    match kind {
        "dnf" | "yum" => {
            args.extend(
                ["install", "-y", "gcc", "make", "elfutils-libelf-devel"].map(str::to_string),
            );
            if need_headers {
                args.push(format!("kernel-devel-{kernel}"));
            }
        }
        "apt-get" => {
            host_success(&path, &os_args(&["update"]))?;
            args.extend(["install", "-y", "build-essential"].map(str::to_string));
            if need_headers {
                args.push(format!("linux-headers-{kernel}"));
            }
        }
        "zypper" => {
            args.extend(["--non-interactive", "install", "gcc", "make"].map(str::to_string));
            if need_headers {
                args.extend(["kernel-devel", "kernel-default-devel"].map(str::to_string));
            }
        }
        "apk" => {
            args.extend(["add", "build-base"].map(str::to_string));
            if need_headers {
                args.push("linux-headers".to_string());
            }
        }
        _ => unreachable!(),
    }
    let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
    let mut command = Command::new(&path);
    command.args(&args);
    if kind == "apt-get" {
        command.env("DEBIAN_FRONTEND", "noninteractive");
    }
    configure_host_command(&mut command);
    let status = command.status()?;
    if !status.success() {
        return Err(format!("host package installation failed with {status}").into());
    }
    Ok(())
}

fn kernel_build_tree(kernel: &str) -> Option<PathBuf> {
    [
        format!("/lib/modules/{kernel}/build"),
        format!("/usr/src/kernels/{kernel}"),
        format!("/usr/src/linux-headers-{kernel}"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|candidate| {
        host_path(candidate)
            .map(|path| path.join("Makefile").is_file())
            .unwrap_or(false)
    })
}

fn parse_module_parameters(path: &Path) -> Result<Vec<OsString>> {
    let text = fs::read_to_string(path)?;
    let mut parameters = Vec::new();
    let mut transport_count = 0_u32;
    for original in text.lines() {
        let parameter = original.strip_suffix('\r').unwrap_or(original);
        if parameter.is_empty() || parameter.starts_with('#') {
            continue;
        }
        if !parameter
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.,:/+=-".contains(&byte))
        {
            return Err(format!("unsafe module parameter: {parameter}").into());
        }
        let Some((name, _)) = parameter.split_once('=') else {
            return Err(format!(
                "module parameters must be one name=value token per line: {parameter}"
            )
            .into());
        };
        if name.is_empty()
            || !name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            })
        {
            return Err(format!(
                "module parameters must be one name=value token per line: {parameter}"
            )
            .into());
        }
        if parameter == "transport=shm" {
            transport_count += 1;
        } else if parameter.starts_with("transport=") {
            return Err("the zcnblk kernel client edge must use transport=shm; TCP and RDMA belong to the separate userspace backplane stage".into());
        }
        parameters.push(OsString::from(parameter));
    }
    if parameters.is_empty() {
        return Err("no zcnblk module parameters were configured".into());
    }
    if transport_count != 1 {
        return Err(
            "module parameters must contain exactly one transport=shm client-edge declaration"
                .into(),
        );
    }
    Ok(parameters)
}

fn oci_architecture(machine: &str) -> Result<&'static str> {
    match machine {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        "riscv64" => Ok("riscv64"),
        "ppc64le" => Ok("ppc64le"),
        _ => Err(format!("unsupported host architecture {machine:?}").into()),
    }
}

fn verify_module_compatibility(module: &Path, identity: &NodeIdentity) -> Result<()> {
    let observed = inspect_kernel_module(module)
        .map_err(|error| format!("inspect kernel module {}: {error}", module.display()))?;
    if observed.module_name != MODULE_NAME {
        return Err(format!(
            "module name is {:?}, expected {MODULE_NAME}",
            observed.module_name
        )
        .into());
    }
    let expected_architecture = oci_architecture(&identity.architecture)?;
    if observed.architecture != expected_architecture {
        return Err(format!(
            "module architecture is {:?}, expected {:?} for host {:?}",
            observed.architecture, expected_architecture, identity.architecture
        )
        .into());
    }
    if observed.kernel_release != identity.kernel {
        return Err(format!(
            "module vermagic {:?} does not match running kernel {:?}",
            observed.vermagic, identity.kernel
        )
        .into());
    }
    Ok(())
}

fn module_parameter_string(parameters: &[OsString]) -> Result<CString> {
    let mut value = String::new();
    for parameter in parameters {
        let parameter = parameter
            .to_str()
            .ok_or("module parameter is not valid UTF-8")?;
        if !value.is_empty() {
            value.push(' ');
        }
        value.push_str(parameter);
    }
    Ok(CString::new(value)?)
}

fn finit_module_direct(module: &Path, parameters: &[OsString]) -> Result<()> {
    let module = File::open(module)?;
    let parameters = module_parameter_string(parameters)?;
    // SAFETY: the file descriptor remains open for the syscall, `parameters`
    // is NUL terminated, and flags=0 requests the kernel's normal validation.
    let result = unsafe {
        libc::syscall(
            libc::SYS_finit_module,
            module.as_raw_fd(),
            parameters.as_ptr(),
            0,
        )
    };
    if result != 0 {
        return Err(format!("finit_module failed: {}", io::Error::last_os_error()).into());
    }
    Ok(())
}

fn wait_for_edge(identity: &NodeIdentity, source: &str, already_loaded: bool) -> Result<()> {
    for _ in 0..100 {
        let block = fs::metadata("/host/dev/zcnblk0")
            .map(|value| value.file_type().is_block_device())
            .unwrap_or(false);
        let control = fs::metadata("/host/dev/zcnblk-shmctl")
            .map(|value| value.file_type().is_char_device())
            .unwrap_or(false);
        if block && control {
            println!(
                "ZCNBLK_NODE_SETUP_READY kernel={} arch={} source={} block=/dev/zcnblk0 control=/dev/zcnblk-shmctl already_loaded={already_loaded}",
                identity.kernel, identity.architecture, source
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("zcnblk_client_mod loaded without publishing /dev/zcnblk0 and /dev/zcnblk-shmctl".into())
}

fn module_is_loaded() -> Result<bool> {
    let modules = fs::read_to_string("/host/proc/modules")?;
    Ok(modules
        .lines()
        .any(|line| line.split_ascii_whitespace().next() == Some(MODULE_NAME)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModuleSource {
    Host,
    Image,
    Http,
    Build,
}

impl ModuleSource {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "host" => Ok(Self::Host),
            "image" => Ok(Self::Image),
            "http" => Ok(Self::Http),
            "build" => Ok(Self::Build),
            _ => Err("module source type must be host, image, http, or build".into()),
        }
    }
}

fn http_client(url: &Url) -> Result<Client> {
    let secure_only = url.scheme() == "https";
    let connect = env_positive_u64("ZCNBLK_MODULE_HTTP_CONNECT_TIMEOUT_SECONDS", 5)?;
    let total = env_positive_u64("ZCNBLK_MODULE_HTTP_TOTAL_TIMEOUT_SECONDS", 60)?;
    let policy = Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("module download exceeded ten redirects");
        }
        let allowed = if secure_only {
            attempt.url().scheme() == "https"
        } else {
            matches!(attempt.url().scheme(), "http" | "https")
        };
        if allowed {
            attempt.follow()
        } else {
            attempt.error("module download redirect changed to a disallowed URL scheme")
        }
    });
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(connect))
        .timeout(Duration::from_secs(total))
        .redirect(policy)
        .build()?)
}

fn validated_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    match url.scheme() {
        "https" => Ok(url),
        "http" if env_bool("ZCNBLK_MODULE_HTTP_ALLOW_INSECURE", false)? => Ok(url),
        "http" => Err(format!("plain HTTP requires allowInsecureHttp=true: {value}").into()),
        _ => Err(format!("module artifact URL must use http:// or https://: {value}").into()),
    }
}

fn response(url: &str) -> Result<Response> {
    let url = validated_url(url)?;
    Ok(http_client(&url)?.get(url).send()?.error_for_status()?)
}

fn download_file(url: &str, destination: &Path) -> Result<()> {
    let mut response = response(url)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    io::copy(&mut response, &mut file)?;
    file.flush()?;
    set_readable_mode(destination)?;
    Ok(())
}

fn download_checksum(url: &str) -> Result<String> {
    let mut response = response(url)?.take(4097);
    let mut bytes = Vec::new();
    response.read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err("downloaded checksum response exceeds 4096 bytes".into());
    }
    let text = std::str::from_utf8(&bytes)?;
    let value = text
        .split_ascii_whitespace()
        .next()
        .ok_or("downloaded checksum response is empty")?;
    validate_sha256("downloaded-checksum", value)
}

fn fetch_expected_checksum(identity: &NodeIdentity) -> Result<String> {
    let configured = env_value("ZCNBLK_MODULE_HTTP_SHA256", "");
    if !configured.is_empty() {
        return validate_sha256("moduleSource.http.sha256", &configured);
    }
    let template = env_value("ZCNBLK_MODULE_HTTP_CHECKSUM_URL_TEMPLATE", "");
    if template.is_empty() {
        return Err("HTTP mode requires a pinned SHA-256 or checksum URL".into());
    }
    download_checksum(&expand_template(&template, identity))
}

fn fetch_module_atomic(url: &str, destination: &Path, expected: &str) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "module destination has no parent: {}",
            destination.display()
        )
    })?;
    fs::create_dir_all(parent)?;
    let partial = unique_partial(destination, "partial");
    let cleanup = RemoveOnDrop(partial.clone());
    download_file(url, &partial)?;
    verify_sha256(&partial, expected)?;
    fs::rename(&partial, destination)?;
    cleanup.disarm();
    Ok(())
}

fn select_http_module(
    identity: &NodeIdentity,
    cache_dir: &Path,
    cached_module: &Path,
) -> Result<(PathBuf, Option<RemoveOnDrop>)> {
    let pinned = env_value("ZCNBLK_MODULE_HTTP_SHA256", "");
    let pinned = if pinned.is_empty() {
        None
    } else {
        Some(validate_sha256("moduleSource.http.sha256", &pinned)?)
    };
    match env_value("ZCNBLK_MODULE_HTTP_DELIVERY", "nodeCacheDaemonSet").as_str() {
        "nodeCacheDaemonSet" => {
            let ready = PathBuf::from(format!("{}.ready", cached_module.display()));
            let ready_host = host_path(&ready)?;
            let module_host = host_path(cached_module)?;
            let wait_seconds = env_positive_u64("ZCNBLK_MODULE_HTTP_CACHE_WAIT_SECONDS", 300)?;
            for _ in 0..wait_seconds.saturating_mul(10) {
                if ready_host.is_file() && module_host.is_file() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if !ready_host.is_file() || !module_host.is_file() {
                return Err(format!(
                    "timed out waiting for HTTP artifact cache at {}",
                    cached_module.display()
                )
                .into());
            }
            let ready_text = fs::read_to_string(&ready_host)?;
            let cached_expected = ready_text
                .split_ascii_whitespace()
                .next()
                .ok_or("cached module ready file is empty")?;
            let cached_expected = validate_sha256("cached-module-ready", cached_expected)?;
            if let Some(expected) = pinned.as_deref() {
                if cached_expected != expected {
                    return Err("cached module digest does not match the Helm-pinned digest".into());
                }
            }
            verify_sha256(&module_host, &cached_expected)?;
            let private = cache_dir.join(format!("zcnblk_client_mod.load.{}", std::process::id()));
            let private_host = host_path(&private)?;
            atomic_copy(&module_host, &private_host, Some(&cached_expected))?;
            Ok((private, Some(RemoveOnDrop(private_host))))
        }
        "direct" => {
            let expected = match pinned {
                Some(value) => value,
                None => fetch_expected_checksum(identity)?,
            };
            let module_host = host_path(cached_module)?;
            fs::create_dir_all(
                module_host
                    .parent()
                    .ok_or("cached module path has no parent")?,
            )?;
            let matches = module_host.is_file() && file_sha256(&module_host)? == expected;
            if matches {
                println!(
                    "zcnblk-node-setup: reusing verified HTTP module cache {}",
                    cached_module.display()
                );
            } else {
                let template = env_value("ZCNBLK_MODULE_HTTP_URL_TEMPLATE", "");
                let url = expand_template(&template, identity);
                fetch_module_atomic(&url, &module_host, &expected)?;
            }
            Ok((cached_module.to_path_buf(), None))
        }
        _ => Err("unsupported HTTP delivery mode".into()),
    }
}

fn select_build_module(identity: &NodeIdentity) -> Result<PathBuf> {
    if !env_bool("ZCNBLK_DEVELOPMENT_BUILD_ENABLED", false)? {
        return Err("on-node compilation requires explicit developmentBuild.enabled=true".into());
    }
    let build_root = PathBuf::from(env_value(
        "ZCNBLK_DEVELOPMENT_BUILD_ROOT",
        "/var/lib/zccusan/kmods/build",
    ));
    validate_absolute_path("development build root", &build_root)?;
    let build_dir = build_root
        .join(&identity.architecture)
        .join(&identity.kernel);
    let host_build_dir = host_path(&build_dir)?;
    fs::create_dir_all(&host_build_dir)?;
    for file in [
        "zcnblk_client_mod.c",
        "zcnblk_shm_abi.h",
        "Makefile",
        "Kbuild",
    ] {
        atomic_copy(
            &Path::new("/module-source").join(file),
            &host_build_dir.join(file),
            None,
        )?;
    }

    let mut tree = kernel_build_tree(&identity.kernel);
    let mut make = host_command("make");
    let mut compiler = host_command("cc").or_else(|| host_command("gcc"));
    if tree.is_none() || make.is_none() || compiler.is_none() {
        if !env_bool("ZCNBLK_DEVELOPMENT_BUILD_INSTALL_HOST_DEPENDENCIES", false)? {
            return Err("development build requires host headers/tools or developmentBuild.installHostDependencies=true".into());
        }
        install_build_dependencies(&identity.kernel, tree.is_none())?;
        tree = kernel_build_tree(&identity.kernel);
        make = host_command("make");
        compiler = host_command("cc").or_else(|| host_command("gcc"));
    }
    let tree = tree.ok_or_else(|| {
        format!(
            "host package installation did not provide headers for {}",
            identity.kernel
        )
    })?;
    let make = make.ok_or("host package installation did not provide make")?;
    let _compiler = compiler.ok_or("host package installation did not provide a C compiler")?;
    host_success(
        &make,
        &[
            OsString::from("-C"),
            build_dir.as_os_str().to_owned(),
            OsString::from(format!("KDIR={}", tree.display())),
            OsString::from("all"),
        ],
    )?;
    let module = build_dir.join(MODULE_FILE);
    if !host_path(&module)?.is_file() {
        return Err(format!("kernel build did not produce {}", module.display()).into());
    }
    Ok(module)
}

fn run_setup() -> Result<()> {
    let identity = node_identity()?;
    let source_text = env_value("ZCNBLK_MODULE_SOURCE_TYPE", "");
    let source = ModuleSource::parse(&source_text)?;
    let parameters_file = PathBuf::from(env_value(
        "ZCNBLK_PARAMETERS_FILE",
        "/node-setup/module-parameters",
    ));
    if !parameters_file.is_file() {
        return Err(format!(
            "module parameter file is not readable: {}",
            parameters_file.display()
        )
        .into());
    }
    if module_is_loaded()? {
        return wait_for_edge(&identity, "already-loaded", true);
    }

    let cache_root = PathBuf::from(env_value(
        "ZCNBLK_MODULE_CACHE_ROOT",
        "/var/lib/zccusan/kmods",
    ));
    validate_absolute_path("module cache root", &cache_root)?;
    let cache_dir = cache_root
        .join(&identity.architecture)
        .join(&identity.kernel);
    let cached_module = cache_dir.join(MODULE_FILE);
    let configured_sha = env_value("ZCNBLK_MODULE_SHA256", "");
    let configured_sha = if configured_sha.is_empty() {
        None
    } else {
        Some(validate_sha256("moduleSource.sha256", &configured_sha)?)
    };

    let mut cleanup = None;
    let module_path = match source {
        ModuleSource::Host => {
            let template = env_value("ZCNBLK_MODULE_HOST_PATH_TEMPLATE", "");
            let path = PathBuf::from(expand_template(&template, &identity));
            validate_absolute_path("host module path", &path)?;
            if !host_path(&path)?.is_file() {
                return Err(
                    format!("configured host module does not exist: {}", path.display()).into(),
                );
            }
            path
        }
        ModuleSource::Image => {
            let template = env_value("ZCNBLK_MODULE_IMAGE_PATH_TEMPLATE", "");
            let image_path = PathBuf::from(expand_template(&template, &identity));
            validate_absolute_path("module image path", &image_path)?;
            if !image_path.is_file() {
                return Err(format!(
                    "node-setup image does not contain module artifact: {}",
                    image_path.display()
                )
                .into());
            }
            atomic_copy(
                &image_path,
                &host_path(&cached_module)?,
                configured_sha.as_deref(),
            )?;
            cached_module.clone()
        }
        ModuleSource::Http => {
            let (path, guard) = select_http_module(&identity, &cache_dir, &cached_module)?;
            cleanup = guard;
            path
        }
        ModuleSource::Build => select_build_module(&identity)?,
    };

    if source != ModuleSource::Image {
        if let Some(expected) = configured_sha.as_deref() {
            verify_sha256(&host_path(&module_path)?, expected)?;
        }
    }

    let module_host_path = host_path(&module_path)?;
    verify_module_compatibility(&module_host_path, &identity)?;

    let mut insmod = host_command("insmod");
    if insmod.is_none() && env_bool("ZCNBLK_INSTALL_MODULE_TOOLS", false)? {
        install_module_tools()?;
        insmod = host_command("insmod");
    }
    if let Some(modprobe) = host_command("modprobe") {
        for dependency in ["authenc", "gcm", "sha256_generic"] {
            let _ = host_output(&modprobe, &[OsString::from(dependency)]);
        }
    }

    let parameters = parse_module_parameters(&parameters_file)?;
    if let Some(insmod) = insmod {
        let mut arguments = vec![module_path.as_os_str().to_owned()];
        arguments.extend(parameters);
        host_success(&insmod, &arguments)?;
    } else {
        println!(
            "zcnblk-node-setup: host insmod is absent; loading verified module with finit_module"
        );
        finit_module_direct(&module_host_path, &parameters)?;
    }
    drop(cleanup);
    wait_for_edge(&identity, &source_text, false)
}

fn write_ready_atomic(path: &Path, expected: &str) -> Result<()> {
    let partial = unique_partial(path, "partial");
    let cleanup = RemoveOnDrop(partial.clone());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)?;
    writeln!(file, "{expected}")?;
    file.flush()?;
    set_readable_mode(&partial)?;
    fs::rename(&partial, path)?;
    cleanup.disarm();
    Ok(())
}

fn cache_once(identity: &NodeIdentity, cache_mount: &Path, cache_root: &Path) -> Result<()> {
    let cache_dir = cache_mount
        .join(&identity.architecture)
        .join(&identity.kernel);
    fs::create_dir_all(&cache_dir)?;
    let module_path = cache_dir.join(MODULE_FILE);
    let ready_path = PathBuf::from(format!("{}.ready", module_path.display()));
    let expected = fetch_expected_checksum(identity)?;
    let matches = module_path.is_file() && file_sha256(&module_path)? == expected;
    if !matches {
        let template = env_value("ZCNBLK_MODULE_HTTP_URL_TEMPLATE", "");
        let url = expand_template(&template, identity);
        fetch_module_atomic(&url, &module_path, &expected)?;
    }
    write_ready_atomic(&ready_path, &expected)?;
    println!(
        "ZCNBLK_MODULE_CACHE_READY kernel={} arch={} sha256={} path={}",
        identity.kernel,
        identity.architecture,
        expected,
        cache_root
            .join(&identity.architecture)
            .join(&identity.kernel)
            .join(MODULE_FILE)
            .display()
    );
    Ok(())
}

fn run_cache() -> Result<()> {
    let identity = node_identity()?;
    let cache_root = PathBuf::from(env_value(
        "ZCNBLK_MODULE_CACHE_ROOT",
        "/var/lib/zccusan/kmods",
    ));
    validate_absolute_path("module cache root", &cache_root)?;
    let cache_mount = PathBuf::from(env_value("ZCNBLK_MODULE_CACHE_MOUNT", "/module-cache"));
    validate_absolute_path("module cache mount", &cache_mount)?;
    let refresh = env_positive_u64("ZCNBLK_MODULE_HTTP_REFRESH_SECONDS", 300)?;
    loop {
        cache_once(&identity, &cache_mount, &cache_root)?;
        thread::sleep(Duration::from_secs(refresh));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_supported_identity_tokens() {
        let identity = NodeIdentity {
            kernel: "6.12.1-test".to_string(),
            architecture: "x86_64".to_string(),
        };
        assert_eq!(
            expand_template("/k/%ARCH%/%KERNEL_RELEASE%/m.ko", &identity),
            "/k/x86_64/6.12.1-test/m.ko"
        );
    }

    #[test]
    fn absolute_paths_reject_traversal() {
        assert!(validate_absolute_path("test", Path::new("/safe/path")).is_ok());
        assert!(validate_absolute_path("test", Path::new("relative/path")).is_err());
        assert!(validate_absolute_path("test", Path::new("/safe/../escape")).is_err());
    }

    #[test]
    fn validates_module_parameters_and_transport_boundary() {
        let base = env::temp_dir().join(format!("zcblock-node-setup-test-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let valid = base.join("valid");
        fs::write(&valid, "transport=shm\nlanes=4\n# comment\n").unwrap();
        assert_eq!(parse_module_parameters(&valid).unwrap().len(), 2);
        let invalid = base.join("invalid");
        fs::write(&invalid, "transport=tcp\n").unwrap();
        assert!(parse_module_parameters(&invalid).is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn maps_uname_architectures_to_oci_architectures() {
        assert_eq!(oci_architecture("x86_64").unwrap(), "amd64");
        assert_eq!(oci_architecture("aarch64").unwrap(), "arm64");
        assert!(oci_architecture("mystery64").is_err());
    }

    #[test]
    fn finit_module_parameters_are_space_delimited_and_nul_terminated() {
        let parameters = vec![OsString::from("transport=shm"), OsString::from("lanes=4")];
        let encoded = module_parameter_string(&parameters).unwrap();
        assert_eq!(encoded.to_bytes_with_nul(), b"transport=shm lanes=4\0");
    }

    #[test]
    fn sha256_validation_is_exact_and_normalized() {
        let upper = "A".repeat(64);
        assert_eq!(validate_sha256("test", &upper).unwrap(), "a".repeat(64));
        assert!(validate_sha256("test", "abcd").is_err());
    }
}
