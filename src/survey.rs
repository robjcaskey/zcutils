use std::collections::VecDeque;
use std::env;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use reqwest::header::{self, HeaderValue};
use serde_json::{self, Value, json};

const SURVEY_QUEUE_MAX_BYTES: usize = 256 * 1024;
const TELEMETRY_EVENT_MAX_BYTES: usize = 4 * 1024;
const SURVEY_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const SURVEY_REQUEST_TIMEOUT: Duration = Duration::from_millis(350);
const SURVEY_REQUEST_CONNECT_TIMEOUT: Duration = Duration::from_millis(120);
const SURVEY_SHUTDOWN_MAX_WAIT: Duration = Duration::from_millis(1_500);
const MANAGEMENT_ENABLED_ENV: &str = "ZCCUSAN_MANAGEMENT_CHECKIN_ENABLED";
const MANAGEMENT_URL_ENV: &str = "ZCCUSAN_MANAGEMENT_CHECKIN_URL";
const SURVEY_ENABLED_ENV: &str = "ZCCUSAN_SURVEY_ENABLED";
const SURVEY_BACKEND_URL_ENV: &str = "ZCCUSAN_SURVEY_BACKEND_URL";
pub const DEFAULT_COMMUNITY_SURVEY_URL: &str =
    "https://vdq4ma9dl2.execute-api.us-east-1.amazonaws.com/survey";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReporterRoute {
    Management,
    CommunitySurvey,
}

#[derive(Clone, Debug)]
pub struct SurveyReporter {
    enabled: bool,
    inner: Option<Arc<ReporterInner>>,
}

#[derive(Debug)]
struct ReporterInner {
    state: Arc<(Mutex<SurveyQueue>, Condvar)>,
    sender: Mutex<Option<thread::JoinHandle<()>>>,
}

impl SurveyReporter {
    pub fn new() -> Self {
        let management_enabled = parse_env_enabled(MANAGEMENT_ENABLED_ENV, true);
        let management_url = read_http_url(MANAGEMENT_URL_ENV);
        let survey_enabled = parse_env_enabled(SURVEY_ENABLED_ENV, true);
        let survey_url = read_http_url(SURVEY_BACKEND_URL_ENV)
            .or_else(|| Some(DEFAULT_COMMUNITY_SURVEY_URL.to_string()));
        let Some((backend_url, _route)) = select_backend(
            management_enabled,
            management_url,
            survey_enabled,
            survey_url,
        ) else {
            return Self {
                enabled: false,
                inner: None,
            };
        };

        Self::with_backend(backend_url)
    }

    fn with_backend(backend_url: String) -> Self {
        let state = Arc::new((Mutex::new(SurveyQueue::new()), Condvar::new()));
        let queue_thread = Arc::clone(&state);
        let sender_url = backend_url;

        let sender = match thread::Builder::new()
            .name("zcutils-telemetry-sender".to_string())
            .spawn(move || {
                sender_loop(queue_thread, sender_url);
            }) {
            Ok(handle) => Some(handle),
            Err(_) => None,
        };

        if sender.is_none() {
            return Self {
                enabled: false,
                inner: None,
            };
        }

        Self {
            enabled: true,
            inner: Some(Arc::new(ReporterInner {
                state,
                sender: Mutex::new(sender),
            })),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn emit_stream(&self, events: Vec<Value>) {
        if !self.enabled {
            return;
        }

        let Some(inner) = self.inner.as_ref() else {
            return;
        };

        let (mutex, cvar) = inner.state.as_ref();
        let environment_id = survey_environment_id();
        for mut event in events {
            if let (Some(environment_id), Value::Object(payload)) =
                (environment_id.as_ref(), &mut event)
            {
                payload
                    .entry("environment_id".to_string())
                    .or_insert_with(|| json!(environment_id));
            }
            let Ok(event_bytes) = serde_json::to_vec(&event) else {
                continue;
            };

            if event_bytes.len() > TELEMETRY_EVENT_MAX_BYTES {
                continue;
            }
            let Some(mut queue) = try_lock_mutex(mutex) else {
                continue;
            };
            if queue.enqueue(event_bytes) {
                cvar.notify_one();
            }
        }
    }

    pub fn emit_event(&self, event_type: &str, payload: serde_json::Map<String, Value>) {
        if !self.enabled {
            return;
        }

        let Some(inner) = self.inner.as_ref() else {
            return;
        };

        let mut event_payload = payload;
        event_payload.insert("event_type".to_string(), json!(event_type));
        event_payload.insert("event_at_ms".to_string(), json!(event_time_ms()));
        event_payload.insert("cloud_region".to_string(), json!(survey_region()));
        if let Some(environment_id) = survey_environment_id() {
            event_payload.insert("environment_id".to_string(), json!(environment_id));
        }

        let Ok(event_bytes) = serde_json::to_vec(&Value::Object(event_payload)) else {
            return;
        };
        if event_bytes.len() > TELEMETRY_EVENT_MAX_BYTES {
            return;
        }

        let (mutex, cvar) = inner.state.as_ref();
        let Some(mut queue) = try_lock_mutex(mutex) else {
            return;
        };
        if queue.enqueue(event_bytes) {
            cvar.notify_one();
        }
    }

    pub fn shutdown(&self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let deadline = Instant::now() + SURVEY_SHUTDOWN_MAX_WAIT;

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
}

fn select_backend(
    management_enabled: bool,
    management_url: Option<String>,
    survey_enabled: bool,
    survey_url: Option<String>,
) -> Option<(String, ReporterRoute)> {
    if management_enabled && let Some(url) = management_url {
        return Some((url, ReporterRoute::Management));
    }
    if survey_enabled && let Some(url) = survey_url {
        return Some((url, ReporterRoute::CommunitySurvey));
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

fn survey_region() -> String {
    env::var("AWS_REGION")
        .or_else(|_| env::var("AWS_DEFAULT_REGION"))
        .or_else(|_| env::var("CLOUD_REGION"))
        .unwrap_or_default()
}

fn survey_environment_id() -> Option<String> {
    env::var("ZCCU_ENVIRONMENT_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn is_http_url(value: &str) -> bool {
    !value.is_empty() && (value.starts_with("https://") || value.starts_with("http://"))
}

fn event_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_millis())
}

fn sender_loop(queue: Arc<(Mutex<SurveyQueue>, Condvar)>, backend_url: String) {
    let client = match Client::builder()
        .timeout(SURVEY_REQUEST_TIMEOUT)
        .connect_timeout(SURVEY_REQUEST_CONNECT_TIMEOUT)
        .pool_max_idle_per_host(0)
        .http1_only()
        .build()
    {
        Ok(client) => client,
        Err(_) => return,
    };

    let close_conn = HeaderValue::from_static("close");

    loop {
        let batch = {
            let (mutex, cvar) = queue.as_ref();
            let mut queue = lock_mutex(mutex);

            while queue.pending.is_empty() && !queue.stopped {
                match cvar.wait_timeout(queue, SURVEY_FLUSH_INTERVAL) {
                    Ok((next, wait_result)) => {
                        queue = next;
                        if !queue.pending.is_empty() || queue.stopped || wait_result.timed_out() {
                            break;
                        }
                    }
                    Err(_) => {
                        return;
                    }
                }
            }

            if queue.pending.is_empty() {
                if queue.stopped {
                    break;
                } else {
                    continue;
                }
            }

            let mut batch = Vec::new();
            batch.push(b'[');
            while let Some(payload) = queue.pending.pop_front() {
                let separator = if batch.len() == 1 { 0 } else { 1 };
                let projected_len = batch.len() + separator + payload.len() + 1;
                if projected_len > SURVEY_QUEUE_MAX_BYTES {
                    queue.pending.push_front(payload);
                    break;
                }
                if separator == 1 {
                    batch.push(b',');
                }
                batch.extend_from_slice(&payload);
                queue.current_bytes = queue.current_bytes.saturating_sub(payload.len());
            }
            batch.push(b']');
            batch
        };

        if batch.len() < 2 || batch.len() > SURVEY_QUEUE_MAX_BYTES {
            continue;
        }

        let request = client
            .post(&backend_url)
            .header(header::CONNECTION, close_conn.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(batch);
        if let Ok(response) = request.send() {
            drop(response);
        }
    }
}

fn lock_mutex<'a, T>(mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn try_lock_mutex<'a, T>(mutex: &'a Mutex<T>) -> Option<MutexGuard<'a, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[derive(Debug)]
struct SurveyQueue {
    pending: VecDeque<Vec<u8>>,
    current_bytes: usize,
    stopped: bool,
}

impl SurveyQueue {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            current_bytes: 0,
            stopped: false,
        }
    }

    fn enqueue(&mut self, payload: Vec<u8>) -> bool {
        if payload.len() > TELEMETRY_EVENT_MAX_BYTES || payload.len() + 2 > SURVEY_QUEUE_MAX_BYTES {
            return false;
        }

        while self.current_bytes + payload.len() > SURVEY_QUEUE_MAX_BYTES {
            if let Some(dropped) = self.pending.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(dropped.len());
                continue;
            }
            break;
        }

        let payload_len = payload.len();
        self.pending.push_back(payload);
        self.current_bytes += payload_len;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    #[test]
    fn management_collector_takes_precedence_over_direct_survey() {
        assert_eq!(
            select_backend(
                true,
                url("http://telemetry:9899/v1/events"),
                true,
                url("https://survey.example/survey"),
            ),
            Some((
                "http://telemetry:9899/v1/events".to_string(),
                ReporterRoute::Management,
            ))
        );
    }

    #[test]
    fn disabled_management_falls_back_to_direct_survey() {
        assert_eq!(
            select_backend(
                false,
                url("http://telemetry:9899/v1/events"),
                true,
                url("https://survey.example/survey"),
            ),
            Some((
                "https://survey.example/survey".to_string(),
                ReporterRoute::CommunitySurvey,
            ))
        );
    }

    #[test]
    fn missing_management_falls_back_to_direct_survey() {
        assert_eq!(
            select_backend(true, None, true, url("https://survey.example/survey"),),
            Some((
                "https://survey.example/survey".to_string(),
                ReporterRoute::CommunitySurvey,
            ))
        );
    }

    #[test]
    fn survey_opt_out_does_not_disable_management_delivery() {
        assert_eq!(
            select_backend(
                true,
                url("http://telemetry:9899/v1/events"),
                false,
                url("https://survey.example/survey"),
            ),
            Some((
                "http://telemetry:9899/v1/events".to_string(),
                ReporterRoute::Management,
            ))
        );
    }

    #[test]
    fn survey_opt_out_disables_direct_fallback() {
        assert_eq!(
            select_backend(false, None, false, url("https://survey.example/survey"),),
            None
        );
    }

    #[test]
    fn edge_queue_rejects_events_over_four_kibibytes() {
        let mut queue = SurveyQueue::new();
        assert!(queue.enqueue(vec![0; TELEMETRY_EVENT_MAX_BYTES]));
        assert!(!queue.enqueue(vec![0; TELEMETRY_EVENT_MAX_BYTES + 1]));
    }

    #[test]
    fn compiled_default_survey_endpoint_is_https() {
        assert!(is_http_url(DEFAULT_COMMUNITY_SURVEY_URL));
        assert!(DEFAULT_COMMUNITY_SURVEY_URL.starts_with("https://"));
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

        let reporter = SurveyReporter::with_backend(format!("http://{address}/v1/events"));
        reporter.emit_event("shutdown_budget_test", serde_json::Map::new());
        let started = Instant::now();
        reporter.shutdown();

        assert!(started.elapsed() <= Duration::from_millis(1_600));
    }

    #[test]
    fn shutdown_budget_includes_internal_lock_contention() {
        use std::sync::mpsc;

        let reporter = SurveyReporter::with_backend("http://127.0.0.1:9/events".to_string());
        let inner = reporter.inner.as_ref().expect("enabled reporter");
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
