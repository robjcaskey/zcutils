use std::collections::VecDeque;
use std::env;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use reqwest::header::{self, HeaderValue};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::telemetry::{NonIdentifyingTelemetry, TelemetryRecord};

const OUTBOUND_QUEUE_MAX_BYTES: usize = 256 * 1024;
const TELEMETRY_EVENT_MAX_BYTES: usize = 4 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(350);
const REQUEST_CONNECT_TIMEOUT: Duration = Duration::from_millis(120);
const SHUTDOWN_MAX_WAIT: Duration = Duration::from_millis(1_500);
const TELEMETRY_API_ENDPOINT_ENV: &str = "ZCCUSAN_TELEMETRY_API_ENDPOINT";
const COMMUNITY_SURVEY_ENABLED_ENV: &str = "ZCCUSAN_COMMUNITY_SURVEY_ENABLED";
const COMMUNITY_SURVEY_API_ENDPOINT_ENV: &str = "ZCCUSAN_COMMUNITY_SURVEY_API_ENDPOINT";

pub const DEFAULT_COMMUNITY_SURVEY_API_ENDPOINT: &str =
    "https://vdq4ma9dl2.execute-api.us-east-1.amazonaws.com/survey";

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReporterRoute {
    TelemetryApi(String),
    CommunitySurvey(String),
}

/// Nonblocking edge telemetry publisher.
///
/// A configured telemetry API always receives the versioned telemetry record.
/// When no telemetry API is configured, direct community participation first
/// converts the record into the `NonIdentifyingTelemetry` type.
#[derive(Clone, Debug)]
pub struct TelemetryReporter {
    inner: Option<ReporterInner>,
}

#[derive(Clone, Debug)]
enum ReporterInner {
    Telemetry(Arc<TypedReporterInner<TelemetryRecord>>),
    CommunitySurvey(Arc<TypedReporterInner<NonIdentifyingTelemetry>>),
}

#[derive(Debug)]
struct TypedReporterInner<T> {
    state: Arc<(Mutex<OutboundQueue<T>>, Condvar)>,
    sender: Mutex<Option<thread::JoinHandle<()>>>,
}

trait WireRecord: Send + 'static {
    fn to_json_bytes(&self) -> Vec<u8>;
}

impl WireRecord for TelemetryRecord {
    fn to_json_bytes(&self) -> Vec<u8> {
        TelemetryRecord::to_json_bytes(self)
    }
}

impl WireRecord for NonIdentifyingTelemetry {
    fn to_json_bytes(&self) -> Vec<u8> {
        NonIdentifyingTelemetry::to_json_bytes(self)
    }
}

impl TelemetryReporter {
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn new() -> Self {
        let route = select_route(
            read_http_url(TELEMETRY_API_ENDPOINT_ENV),
            parse_env_enabled(COMMUNITY_SURVEY_ENABLED_ENV, true),
            read_http_url(COMMUNITY_SURVEY_API_ENDPOINT_ENV)
                .or_else(|| Some(DEFAULT_COMMUNITY_SURVEY_API_ENDPOINT.to_string())),
        );
        match route {
            Some(ReporterRoute::TelemetryApi(endpoint)) => Self {
                inner: build_typed_reporter(endpoint).map(ReporterInner::Telemetry),
            },
            Some(ReporterRoute::CommunitySurvey(endpoint)) => Self {
                inner: build_typed_reporter(endpoint).map(ReporterInner::CommunitySurvey),
            },
            None => Self { inner: None },
        }
    }

    #[cfg(test)]
    fn with_telemetry_api_endpoint(endpoint: String) -> Self {
        Self {
            inner: build_typed_reporter(endpoint).map(ReporterInner::Telemetry),
        }
    }

    #[cfg(test)]
    fn with_community_survey_endpoint(endpoint: String) -> Self {
        Self {
            inner: build_typed_reporter(endpoint).map(ReporterInner::CommunitySurvey),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn emit_stream(&self, events: Vec<Value>) {
        for mut event in events {
            add_installation_id(&mut event);
            let Some(record) = TelemetryRecord::from_value(event) else {
                continue;
            };
            self.enqueue_record(record);
        }
    }

    pub fn emit_event(&self, event_type: &str, mut fields: Map<String, Value>) {
        fields.insert("event_at_ms".to_string(), json!(event_time_ms()));
        let (cloud_provider, cloud_region) = telemetry_cloud_context();
        fields.insert("cloud_provider".to_string(), json!(cloud_provider));
        fields.insert("cloud_region".to_string(), json!(cloud_region));
        fields
            .entry("version".to_string())
            .or_insert_with(|| json!(env!("CARGO_PKG_VERSION")));
        if let Some(installation_id) = telemetry_installation_id() {
            fields
                .entry("installation_id".to_string())
                .or_insert_with(|| json!(installation_id));
        }
        self.enqueue_record(TelemetryRecord::current(event_type, fields));
    }

    fn enqueue_record(&self, record: TelemetryRecord) {
        match self.inner.as_ref() {
            Some(ReporterInner::Telemetry(inner)) => enqueue_nonblocking(inner, record),
            Some(ReporterInner::CommunitySurvey(inner)) => {
                enqueue_nonblocking(inner, record.anonymize())
            }
            None => {}
        }
    }

    pub fn shutdown(&self) {
        match self.inner.as_ref() {
            Some(ReporterInner::Telemetry(inner)) => shutdown_typed(inner),
            Some(ReporterInner::CommunitySurvey(inner)) => shutdown_typed(inner),
            None => {}
        }
    }
}

impl Default for TelemetryReporter {
    fn default() -> Self {
        Self::new()
    }
}

fn build_typed_reporter<T: WireRecord>(endpoint: String) -> Option<Arc<TypedReporterInner<T>>> {
    let state = Arc::new((Mutex::new(OutboundQueue::new()), Condvar::new()));
    let sender_state = Arc::clone(&state);
    let sender = thread::Builder::new()
        .name("zcutils-telemetry-sender".to_string())
        .spawn(move || sender_loop(sender_state, endpoint))
        .ok()?;
    Some(Arc::new(TypedReporterInner {
        state,
        sender: Mutex::new(Some(sender)),
    }))
}

fn enqueue_nonblocking<T: WireRecord>(inner: &TypedReporterInner<T>, record: T) {
    let (mutex, cvar) = inner.state.as_ref();
    let Some(mut queue) = try_lock_mutex(mutex) else {
        return;
    };
    if queue.enqueue(record) {
        cvar.notify_one();
    }
}

fn shutdown_typed<T>(inner: &TypedReporterInner<T>) {
    let deadline = Instant::now() + SHUTDOWN_MAX_WAIT;
    let (mutex, cvar) = inner.state.as_ref();
    let mut stop_signalled = false;
    while Instant::now() < deadline {
        if let Some(mut queue) = try_lock_mutex(mutex) {
            queue.stopped = true;
            cvar.notify_one();
            stop_signalled = true;
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    if !stop_signalled {
        return;
    }

    let mut sender = None;
    while Instant::now() < deadline {
        if let Some(mut sender_slot) = try_lock_mutex(&inner.sender) {
            sender = sender_slot.take();
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let Some(sender) = sender else {
        return;
    };
    while !sender.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if sender.is_finished() {
        let _ = sender.join();
    }
}

fn select_route(
    telemetry_api_endpoint: Option<String>,
    community_survey_enabled: bool,
    community_survey_api_endpoint: Option<String>,
) -> Option<ReporterRoute> {
    if let Some(endpoint) = telemetry_api_endpoint {
        return Some(ReporterRoute::TelemetryApi(endpoint));
    }
    if community_survey_enabled && let Some(endpoint) = community_survey_api_endpoint {
        return Some(ReporterRoute::CommunitySurvey(endpoint));
    }
    None
}

fn read_http_url(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| is_http_url(value))
}

fn parse_env_enabled(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .and_then(|value| parse_enabled_value(&value))
        .unwrap_or(default)
}

fn parse_enabled_value(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

fn telemetry_cloud_context() -> (String, String) {
    let region = env::var("AWS_REGION")
        .or_else(|_| env::var("AWS_DEFAULT_REGION"))
        .or_else(|_| env::var("CLOUD_REGION"))
        .unwrap_or_default();
    let provider = env::var("CLOUD_PROVIDER").unwrap_or_else(|_| {
        if env::var_os("AWS_REGION").is_some() || env::var_os("AWS_DEFAULT_REGION").is_some() {
            "aws".to_string()
        } else if env::var_os("AZURE_HTTP_USER_AGENT").is_some() {
            "azure".to_string()
        } else if env::var_os("GOOGLE_CLOUD_PROJECT").is_some() {
            "gcp".to_string()
        } else {
            String::new()
        }
    });
    if !provider.is_empty() && !region.is_empty() {
        return (provider, region);
    }

    // The direct-survey fallback runs outside the measured interval. Query
    // only EC2's identity document, with the short IMDSv2 timeouts enforced by
    // Ec2Imds, rather than sending an instance ID, AZ, or placement-group name.
    use crate::cloud_topology::MetadataSource as _;
    if let Ok(metadata) = crate::cloud_topology::Ec2Imds::connect()
        && let Ok(Some(document)) = metadata.get("dynamic/instance-identity/document")
        && let Ok(document) = serde_json::from_str::<Value>(&document)
    {
        let detected_region = document
            .get("region")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return (
            if provider.is_empty() {
                "aws".to_string()
            } else {
                provider
            },
            if region.is_empty() {
                detected_region
            } else {
                region
            },
        );
    }
    (provider, region)
}

fn telemetry_installation_id() -> Option<String> {
    env::var("ZCCUSAN_INSTALLATION_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(local_anonymous_source_id)
}

/// Return a stable, already one-way local identity when an operator has not
/// supplied an installation ID. This lets ordinary CLI and raw-volume tools
/// appear as one environment in Community Pulse without transmitting the
/// host's machine-id. Direct survey delivery hashes this opaque value again at
/// the `NonIdentifyingTelemetry` boundary.
fn local_anonymous_source_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .into_iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| {
            let mut digest = Sha256::new();
            digest.update(b"zccusan-local-installation-v1\0");
            digest.update(value.as_bytes());
            format!("local-{:x}", digest.finalize())
        })
}

fn add_installation_id(event: &mut Value) {
    if let (Some(installation_id), Value::Object(fields)) = (telemetry_installation_id(), event) {
        fields
            .entry("installation_id".to_string())
            .or_insert_with(|| json!(installation_id));
    }
}

fn is_http_url(value: &str) -> bool {
    !value.is_empty() && (value.starts_with("https://") || value.starts_with("http://"))
}

fn event_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn sender_loop<T: WireRecord>(queue: Arc<(Mutex<OutboundQueue<T>>, Condvar)>, endpoint: String) {
    let client = match Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(REQUEST_CONNECT_TIMEOUT)
        .pool_max_idle_per_host(0)
        .http1_only()
        .build()
    {
        Ok(client) => client,
        Err(_) => return,
    };
    let close_connection = HeaderValue::from_static("close");

    loop {
        let batch = {
            let (mutex, cvar) = queue.as_ref();
            let mut queue = lock_mutex(mutex);
            while queue.pending.is_empty() && !queue.stopped {
                queue = match cvar.wait_timeout(queue, FLUSH_INTERVAL) {
                    Ok((next, _)) => next,
                    Err(_) => return,
                };
            }
            if queue.pending.is_empty() && queue.stopped {
                break;
            }
            queue.take_batch()
        };

        if batch.len() < 2 || batch.len() > OUTBOUND_QUEUE_MAX_BYTES {
            continue;
        }
        let request = client
            .post(&endpoint)
            .header(header::CONNECTION, close_connection.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(batch);
        if let Ok(response) = request.send() {
            drop(response);
        }
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn try_lock_mutex<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[derive(Debug)]
struct OutboundQueue<T> {
    pending: VecDeque<(T, usize)>,
    current_bytes: usize,
    stopped: bool,
}

impl<T> OutboundQueue<T> {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            current_bytes: 0,
            stopped: false,
        }
    }
}

impl<T: WireRecord> OutboundQueue<T> {
    fn enqueue(&mut self, record: T) -> bool {
        let record_bytes = record.to_json_bytes().len();
        if record_bytes == 0
            || record_bytes > TELEMETRY_EVENT_MAX_BYTES
            || record_bytes + 2 > OUTBOUND_QUEUE_MAX_BYTES
        {
            return false;
        }
        while self.current_bytes + record_bytes > OUTBOUND_QUEUE_MAX_BYTES {
            if let Some((_, dropped_bytes)) = self.pending.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(dropped_bytes);
            } else {
                break;
            }
        }
        self.pending.push_back((record, record_bytes));
        self.current_bytes += record_bytes;
        true
    }

    fn take_batch(&mut self) -> Vec<u8> {
        let mut batch = vec![b'['];
        while let Some((record, record_bytes)) = self.pending.pop_front() {
            let wire = record.to_json_bytes();
            let separator = usize::from(batch.len() > 1);
            if batch.len() + separator + wire.len() + 1 > OUTBOUND_QUEUE_MAX_BYTES {
                self.pending.push_front((record, record_bytes));
                break;
            }
            if separator == 1 {
                batch.push(b',');
            }
            batch.extend_from_slice(&wire);
            self.current_bytes = self.current_bytes.saturating_sub(record_bytes);
        }
        batch.push(b']');
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    #[test]
    fn telemetry_api_takes_precedence_over_direct_community_survey() {
        assert_eq!(
            select_route(
                endpoint("http://telemetry:9899/v1/events"),
                true,
                endpoint("https://survey.example/survey"),
            ),
            Some(ReporterRoute::TelemetryApi(
                "http://telemetry:9899/v1/events".to_string()
            ))
        );
    }

    #[test]
    fn missing_telemetry_api_can_use_direct_community_survey() {
        assert_eq!(
            select_route(None, true, endpoint("https://survey.example/survey")),
            Some(ReporterRoute::CommunitySurvey(
                "https://survey.example/survey".to_string()
            ))
        );
    }

    #[test]
    fn community_opt_out_does_not_disable_telemetry_api_delivery() {
        assert_eq!(
            select_route(
                endpoint("http://telemetry:9899/v1/events"),
                false,
                endpoint("https://survey.example/survey"),
            ),
            Some(ReporterRoute::TelemetryApi(
                "http://telemetry:9899/v1/events".to_string()
            ))
        );
    }

    #[test]
    fn community_opt_out_disables_only_direct_fallback() {
        assert_eq!(
            select_route(None, false, endpoint("https://survey.example/survey")),
            None
        );
    }

    #[test]
    fn community_queue_type_contains_only_anonymized_records() {
        let mut queue: OutboundQueue<NonIdentifyingTelemetry> = OutboundQueue::new();
        let raw = TelemetryRecord::from_value(json!({
            "event_type": "test",
            "environment_id": "private-id",
            "volume_id": "private-volume",
        }))
        .expect("record");
        assert!(queue.enqueue(raw.anonymize()));
        let batch = String::from_utf8(queue.take_batch()).expect("JSON UTF-8");
        assert!(!batch.contains("private-id"));
        assert!(!batch.contains("private-volume"));
        assert!(batch.contains("anonymous_installation_id"));
    }

    #[test]
    fn edge_queue_rejects_events_over_four_kibibytes() {
        let mut fields = Map::new();
        fields.insert(
            "future".to_string(),
            json!("x".repeat(TELEMETRY_EVENT_MAX_BYTES)),
        );
        let mut queue: OutboundQueue<TelemetryRecord> = OutboundQueue::new();
        assert!(!queue.enqueue(TelemetryRecord::current("oversized", fields)));
    }

    #[test]
    fn compiled_default_community_endpoint_is_https() {
        assert!(is_http_url(DEFAULT_COMMUNITY_SURVEY_API_ENDPOINT));
        assert!(DEFAULT_COMMUNITY_SURVEY_API_ENDPOINT.starts_with("https://"));
    }

    #[test]
    fn default_local_identity_is_stable_and_does_not_expose_machine_id() {
        let Some(source) = local_anonymous_source_id() else {
            return;
        };
        assert_eq!(Some(source.clone()), local_anonymous_source_id());
        assert!(source.starts_with("local-"));
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(machine_id) = std::fs::read_to_string(path) {
                assert!(!source.contains(machine_id.trim()));
            }
        }
    }

    #[test]
    fn shutdown_never_waits_longer_than_its_fixed_budget() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("test listener address");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                thread::sleep(Duration::from_secs(3));
            }
        });

        let reporter =
            TelemetryReporter::with_telemetry_api_endpoint(format!("http://{address}/v1/events"));
        reporter.emit_event("shutdown_budget_test", Map::new());
        let started = Instant::now();
        reporter.shutdown();
        assert!(started.elapsed() <= Duration::from_millis(1_600));
    }

    #[test]
    fn community_reporter_sends_anonymized_result_before_shutdown() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let address = listener.local_addr().expect("test listener address");
        let (body_tx, body_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept survey request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).expect("read survey request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") && request.ends_with(b"]")
                {
                    break;
                }
            }
            let _ = body_tx.send(request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .expect("write survey response");
        });

        let reporter =
            TelemetryReporter::with_community_survey_endpoint(format!("http://{address}/survey"));
        let mut fields = Map::new();
        fields.insert("installation_id".to_string(), json!("private-host-id"));
        fields.insert("total_iops".to_string(), json!(11_926_000));
        reporter.emit_event("block_benchmark_result", fields);
        reporter.shutdown();

        let request = body_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("community request");
        let request = String::from_utf8(request).expect("HTTP request is UTF-8");
        assert!(request.contains("block_benchmark_result"));
        assert!(request.contains("11926000"));
        assert!(request.contains("anonymous_installation_id"));
        assert!(!request.contains("private-host-id"));
    }

    #[test]
    fn shutdown_budget_includes_internal_lock_contention() {
        use std::sync::mpsc;

        let reporter =
            TelemetryReporter::with_telemetry_api_endpoint("http://127.0.0.1:9/events".to_string());
        let inner = match reporter.inner.as_ref().expect("enabled reporter") {
            ReporterInner::Telemetry(inner) => inner,
            ReporterInner::CommunitySurvey(_) => panic!("expected telemetry route"),
        };
        let queue_guard = lock_mutex(&inner.state.0);
        let shutting_down = reporter.clone();
        let (elapsed_tx, elapsed_rx) = mpsc::channel();
        thread::spawn(move || {
            let started = Instant::now();
            shutting_down.shutdown();
            let _ = elapsed_tx.send(started.elapsed());
        });
        let elapsed = elapsed_rx
            .recv_timeout(Duration::from_millis(1_650))
            .expect("shutdown must honor its total deadline");
        assert!(elapsed <= Duration::from_millis(1_600));
        drop(queue_guard);
        reporter.shutdown();
    }
}
