use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Request {
    Status,
    Freeze,
    Snapshot { snapshot_id: String },
    Thaw,
    Stop,
    Restore { snapshot_id: String },
    Start,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    message: String,
    ready: bool,
    frozen: bool,
    data_files: u64,
    commitlog_files: u64,
    data_digest: String,
    commitlog_digest: String,
}

struct Agent {
    node_id: String,
    ip: String,
    seeds: String,
    root: PathBuf,
    cassandra_home: PathBuf,
    java_home: PathBuf,
    child: Option<Child>,
    frozen: bool,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("agent") if args.len() == 7 => {
            let mut agent = Agent {
                node_id: args[3].clone(),
                ip: args[4].clone(),
                seeds: args[5].clone(),
                root: PathBuf::from(&args[6]),
                cassandra_home: env::var_os("ZC_CASSANDRA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/cassandra")),
                java_home: env::var_os("ZC_JAVA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/java")),
                child: None,
                frozen: false,
            };
            agent.initialize()?;
            agent.start()?;
            run_agent(&args[2], &mut agent)
        }
        Some("control") if args.len() >= 4 => run_control(&args[2], &args[3..]),
        _ => Err(invalid(
            "usage: zccassandra-qemu-agent agent LISTEN NODE_ID IP SEEDS ROOT | control ADDRESS COMMAND [SNAPSHOT_ID]",
        )),
    }
}

impl Agent {
    fn data_dir(&self) -> PathBuf {
        self.root.join("volumes/data")
    }

    fn commitlog_dir(&self) -> PathBuf {
        self.root.join("volumes/commitlog")
    }

    fn conf_dir(&self) -> PathBuf {
        self.root.join("conf")
    }

    fn initialize(&self) -> io::Result<()> {
        fs::create_dir_all(self.data_dir())?;
        fs::create_dir_all(self.commitlog_dir())?;
        fs::create_dir_all(self.root.join("logs"))?;
        fs::create_dir_all(self.root.join("snapshots"))?;
        fs::create_dir_all(self.root.join("heapdump"))?;
        if !self.conf_dir().exists() {
            copy_tree(&self.cassandra_home.join("conf"), &self.conf_dir())?;
        }
        let yaml_path = self.conf_dir().join("cassandra.yaml");
        let mut yaml = fs::read_to_string(&yaml_path)?;
        yaml = yaml
            .replace(
                "cluster_name: 'Test Cluster'",
                "cluster_name: 'zcutils-pitr-qemu'",
            )
            .replace(
                "listen_address: localhost",
                &format!("listen_address: {}", self.ip),
            )
            .replace(
                "rpc_address: localhost",
                &format!("rpc_address: {}", self.ip),
            )
            .replace(
                "- seeds: \"127.0.0.1:7000\"",
                &format!("- seeds: \"{}\"", self.seeds),
            )
            .replace("commitlog_sync: periodic", "commitlog_sync: batch")
            .replace(
                "commitlog_sync_period: 10000ms",
                "# commitlog_sync_period removed for batch mode",
            );
        yaml.push_str(&format!(
            "\n# zcutils QEMU multi-volume ownership.\ndata_file_directories:\n  - '{}'\ncommitlog_directory: '{}'\nsaved_caches_directory: '{}'\nhints_directory: '{}'\n",
            self.data_dir().display(),
            self.commitlog_dir().display(),
            self.data_dir().join("saved_caches").display(),
            self.data_dir().join("hints").display(),
        ));
        fs::write(&yaml_path, yaml)?;
        Ok(())
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env("JAVA_HOME", &self.java_home)
            .env("CASSANDRA_HOME", &self.cassandra_home)
            .env("CASSANDRA_CONF", self.conf_dir())
            .env("CASSANDRA_LOG_DIR", self.root.join("logs"))
            .env("CASSANDRA_HEAPDUMP_DIR", self.root.join("heapdump"))
            .env("MAX_HEAP_SIZE", "512M")
            .env("HEAP_NEWSIZE", "128M")
            .env("MAX_DIRECT_MEMORY_SIZE", "256M")
            .env("LOCAL_JMX", "yes")
            .env("JMX_PORT", "7199")
            .env("MALLOC_ARENA_MAX", "4");
        command
    }

    fn start(&mut self) -> io::Result<()> {
        if self.child.is_some() {
            return Err(invalid("Cassandra is already running"));
        }
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("cassandra-console.log"))?;
        let stderr = stdout.try_clone()?;
        let mut command = self.command(
            self.cassandra_home
                .join("bin/cassandra")
                .to_str()
                .ok_or_else(|| invalid("Cassandra home is not UTF-8"))?,
        );
        let child = command
            .arg("-f")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()?;
        self.child = Some(child);
        self.frozen = false;
        self.wait_ready(Duration::from_secs(180))
    }

    fn wait_ready(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
            {
                self.child = None;
                return Err(invalid(
                    "Cassandra exited before native transport became ready",
                ));
            }
            if TcpStream::connect_timeout(
                &format!("{}:9042", self.ip)
                    .parse::<SocketAddr>()
                    .map_err(|_| invalid("invalid Cassandra IP"))?,
                Duration::from_millis(200),
            )
            .is_ok()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Cassandra readiness timeout",
        ))
    }

    fn signal(&mut self, signal: libc::c_int) -> io::Result<()> {
        let pid = self
            .child
            .as_ref()
            .ok_or_else(|| invalid("Cassandra is not running"))?
            .id();
        let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn freeze(&mut self) -> io::Result<()> {
        if self.frozen {
            return Err(invalid("Cassandra is already frozen"));
        }
        self.signal(libc::SIGSTOP)?;
        self.frozen = true;
        Ok(())
    }

    fn thaw(&mut self) -> io::Result<()> {
        if !self.frozen {
            return Err(invalid("Cassandra is not frozen"));
        }
        self.signal(libc::SIGCONT)?;
        self.frozen = false;
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        if self.frozen {
            self.signal(libc::SIGCONT)?;
            self.frozen = false;
        }
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = child.wait()?;
        Ok(())
    }

    fn snapshot(&self, snapshot_id: &str) -> io::Result<Response> {
        validate_snapshot_id(snapshot_id)?;
        if !self.frozen {
            return Err(invalid("Cassandra must be frozen before snapshot"));
        }
        let target = self.root.join("snapshots").join(snapshot_id);
        if target.exists() {
            return Err(invalid("snapshot already exists"));
        }
        fs::create_dir_all(&target)?;
        copy_tree(&self.data_dir(), &target.join("data"))?;
        copy_tree(&self.commitlog_dir(), &target.join("commitlog"))?;
        sync_tree(&target)?;
        manifest_response(
            "snapshot_synced",
            true,
            &target.join("data"),
            &target.join("commitlog"),
        )
    }

    fn restore(&self, snapshot_id: &str) -> io::Result<Response> {
        validate_snapshot_id(snapshot_id)?;
        if self.child.is_some() {
            return Err(invalid("Cassandra must be stopped before restore"));
        }
        let source = self.root.join("snapshots").join(snapshot_id);
        if !source.is_dir() {
            return Err(invalid("snapshot does not exist"));
        }
        replace_tree(&source.join("data"), &self.data_dir())?;
        replace_tree(&source.join("commitlog"), &self.commitlog_dir())?;
        sync_tree(&self.root.join("volumes"))?;
        manifest_response(
            "restore_synced",
            false,
            &self.data_dir(),
            &self.commitlog_dir(),
        )
    }

    fn response(&mut self, message: &str) -> io::Result<Response> {
        let ready = if self.frozen {
            false
        } else {
            TcpStream::connect_timeout(
                &format!("{}:9042", self.ip)
                    .parse::<SocketAddr>()
                    .map_err(|_| invalid("invalid Cassandra IP"))?,
                Duration::from_millis(100),
            )
            .is_ok()
        };
        manifest_response(message, ready, &self.data_dir(), &self.commitlog_dir()).map(
            |mut value| {
                value.frozen = self.frozen;
                value
            },
        )
    }
}

fn run_agent(listen: &str, agent: &mut Agent) -> io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    println!(
        "CASSANDRA_AGENT_READY node={} ip={} listen={}",
        agent.node_id, agent.ip, listen
    );
    for incoming in listener.incoming() {
        let mut stream = incoming?;
        let mut line = String::new();
        if BufReader::new(stream.try_clone()?).read_line(&mut line)? == 0 {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                write_response(
                    &mut stream,
                    &error_response(format!("decode request: {error}")),
                )?;
                continue;
            }
        };
        let mut shutdown = false;
        let result = (|| -> io::Result<Response> {
            match request {
                Request::Status => agent.response("status"),
                Request::Freeze => {
                    agent.freeze()?;
                    agent.response("frozen")
                }
                Request::Snapshot { snapshot_id } => agent.snapshot(&snapshot_id),
                Request::Thaw => {
                    agent.thaw()?;
                    agent.response("thawed")
                }
                Request::Stop => {
                    agent.stop()?;
                    agent.response("stopped")
                }
                Request::Restore { snapshot_id } => agent.restore(&snapshot_id),
                Request::Start => {
                    agent.start()?;
                    agent.response("started")
                }
                Request::Shutdown => {
                    agent.stop()?;
                    shutdown = true;
                    agent.response("shutdown")
                }
            }
        })();
        let response = result.unwrap_or_else(|error| error_response(error.to_string()));
        write_response(&mut stream, &response)?;
        if shutdown {
            break;
        }
    }
    Ok(())
}

fn run_control(address: &str, args: &[String]) -> io::Result<()> {
    let request = match args.first().map(String::as_str) {
        Some("status") if args.len() == 1 => Request::Status,
        Some("freeze") if args.len() == 1 => Request::Freeze,
        Some("snapshot") if args.len() == 2 => Request::Snapshot {
            snapshot_id: args[1].clone(),
        },
        Some("thaw") if args.len() == 1 => Request::Thaw,
        Some("stop") if args.len() == 1 => Request::Stop,
        Some("restore") if args.len() == 2 => Request::Restore {
            snapshot_id: args[1].clone(),
        },
        Some("start") if args.len() == 1 => Request::Start,
        Some("shutdown") if args.len() == 1 => Request::Shutdown,
        _ => return Err(invalid("invalid control command")),
    };
    let socket: SocketAddr = address
        .parse()
        .map_err(|_| invalid("invalid control address"))?;
    let mut stream = TcpStream::connect_timeout(&socket, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(190)))?;
    serde_json::to_writer(&mut stream, &request).map_err(|error| invalid(error.to_string()))?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Response =
        serde_json::from_str(&line).map_err(|error| invalid(error.to_string()))?;
    println!(
        "{}",
        serde_json::to_string(&response).map_err(|error| invalid(error.to_string()))?
    );
    if response.ok {
        Ok(())
    } else {
        Err(invalid(response.message))
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, response).map_err(|error| invalid(error.to_string()))?;
    stream.write_all(b"\n")
}

fn error_response(message: String) -> Response {
    Response {
        ok: false,
        message,
        ready: false,
        frozen: false,
        data_files: 0,
        commitlog_files: 0,
        data_digest: String::new(),
        commitlog_digest: String::new(),
    }
}

fn manifest_response(
    message: &str,
    ready: bool,
    data: &Path,
    commitlog: &Path,
) -> io::Result<Response> {
    let (data_files, data_digest) = tree_digest(data)?;
    let (commitlog_files, commitlog_digest) = tree_digest(commitlog)?;
    Ok(Response {
        ok: true,
        message: message.into(),
        ready,
        frozen: false,
        data_files,
        commitlog_files,
        data_digest,
        commitlog_digest,
    })
}

fn tree_digest(root: &Path) -> io::Result<(u64, String)> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    for (relative, path) in &files {
        digest.update(relative.as_bytes());
        let mut file = File::open(path)?;
        io::copy(&mut file, &mut digest)?;
    }
    Ok((
        files.len() as u64,
        format!("sha256:{:x}", digest.finalize()),
    ))
}

fn collect_files(root: &Path, path: &Path, output: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let value = entry.path();
        if value.is_dir() {
            collect_files(root, &value, output)?;
        } else if value.is_file() {
            output.push((
                value
                    .strip_prefix(root)
                    .map_err(|_| invalid("manifest path escaped root"))?
                    .to_string_lossy()
                    .into_owned(),
                value,
            ));
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_tree(&source_path, &target_path)?;
        } else if source_path.is_file() {
            fs::copy(&source_path, &target_path)?;
        }
    }
    Ok(())
}

fn replace_tree(source: &Path, target: &Path) -> io::Result<()> {
    if target.exists() {
        fs::remove_dir_all(target)?;
    }
    copy_tree(source, target)
}

fn sync_tree(root: &Path) -> io::Result<()> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    for (_, path) in files {
        File::open(path)?.sync_data()?;
    }
    File::open(root)?.sync_all()
}

fn validate_snapshot_id(value: &str) -> io::Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(invalid("invalid snapshot id"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
