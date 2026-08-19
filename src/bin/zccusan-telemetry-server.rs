use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use std::collections::VecDeque;
use std::env;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zcutils::DEFAULT_COMMUNITY_SURVEY_URL;

const DEFAULT_LISTEN: &str = "0.0.0.0:9899";
const EVENT_BUFFER_CAPACITY_BYTES: usize = 4 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 4 * 1024;
const DEFAULT_FLUSH_INTERVAL_MS: u64 = 750;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 500;
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 150;
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const METRICS_PATH: &str = "/metrics";
const EVENTS_PATH: &str = "/v1/events";

#[derive(Clone, Debug)]
struct Config {
    listen: String,
    upstream_url: Option<String>,
    flush_interval_ms: u64,
    request_timeout_ms: u64,
}

#[derive(Debug)]
struct EventServer {
    queue: VecDeque<EventRecord>,
    queue_bytes: usize,
    dropped_events: u64,
    rejected_events: u64,
    received_events: u64,
    sent_events: u64,
    send_failures: u64,
    last_send_ms: i64,
    next_message_index: u64,
}

#[derive(Clone, Debug)]
struct EventRecord {
    message_index: u64,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct EnqueueResult {
    accepted: usize,
    rejected: usize,
    logged: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct SharedState {
    inner: Mutex<EventServer>,
    ready: Condvar,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_env();
    let state = Arc::new(SharedState {
        inner: Mutex::new(EventServer::new()),
        ready: Condvar::new(),
    });

    spawn_sender_thread(
        Arc::clone(&state),
        cfg.upstream_url.clone(),
        cfg.flush_interval_ms,
        cfg.request_timeout_ms,
    );

    let listener = TcpListener::bind(&cfg.listen)?;
    eprintln!("zccusan telemetry server listening on {}", cfg.listen);
    eprintln!(
        "zccusan telemetry upstream: {}",
        cfg.upstream_url
            .as_deref()
            .unwrap_or("disabled or not configured")
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let _ = handle_connection(stream, state);
                });
            }
            Err(err) => {
                eprintln!("connection accept error: {}", err);
            }
        }
    }

    Ok(())
}

impl Config {
    fn from_env() -> Self {
        let listen =
            env::var("ZCCUSAN_TELEMETRY_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
        let flush_interval_ms = env::var("ZCCUSAN_TELEMETRY_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_FLUSH_INTERVAL_MS);
        let request_timeout_ms = env::var("ZCCUSAN_TELEMETRY_REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);

        let upstream_url = select_upstream(
            env_enabled("ZCCUSAN_SURVEY_ENABLED", true),
            read_http_url("ZCCUSAN_TELEMETRY_UPSTREAM_URL"),
            read_http_url("ZCCUSAN_SURVEY_BACKEND_URL")
                .or_else(|| Some(DEFAULT_COMMUNITY_SURVEY_URL.to_string())),
        );

        Config {
            listen,
            upstream_url,
            flush_interval_ms,
            request_timeout_ms,
        }
    }
}

impl EventServer {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            queue_bytes: 0,
            dropped_events: 0,
            rejected_events: 0,
            received_events: 0,
            sent_events: 0,
            send_failures: 0,
            last_send_ms: -1,
            next_message_index: 1,
        }
    }

    fn current_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().try_into().unwrap_or(i64::MAX))
            .unwrap_or(-1)
    }

    fn enqueue_events(&mut self, events: Vec<Vec<u8>>) -> EnqueueResult {
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        let mut logged = Vec::new();

        for event in events {
            let event_len = event.len();
            if event_len == 0 || event_len > MAX_EVENT_BYTES {
                self.rejected_events = self.rejected_events.saturating_add(1);
                rejected = rejected.saturating_add(1);
                continue;
            }

            let direct_record = self.index_event(event.clone(), self.next_message_index);
            if self.queue_bytes + direct_record.payload.len() <= EVENT_BUFFER_CAPACITY_BYTES {
                self.next_message_index = self.next_message_index.saturating_add(1);
                self.push_record(direct_record.clone());
                logged.push(direct_record.payload);
                self.received_events = self.received_events.saturating_add(1);
                accepted += 1;
                continue;
            }

            let overflow_index = self.next_message_index;
            let event_index = overflow_index.saturating_add(1);
            let indexed_event = self.index_event(event, event_index);
            let mut first_evicted = None;
            let mut last_evicted = None;
            let mut evicted_count = 0u64;
            let mut inserted = false;

            loop {
                let overflow = overflow_event(
                    overflow_index,
                    first_evicted.unwrap_or(0),
                    last_evicted.unwrap_or(0),
                    evicted_count,
                );
                if self.queue_bytes + overflow.payload.len() + indexed_event.payload.len()
                    <= EVENT_BUFFER_CAPACITY_BYTES
                {
                    self.next_message_index = event_index.saturating_add(1);
                    self.push_record(overflow.clone());
                    self.push_record(indexed_event.clone());
                    logged.push(overflow.payload);
                    logged.push(indexed_event.payload);
                    inserted = true;
                    break;
                }

                let Some(evicted) = self.queue.pop_front() else {
                    rejected = rejected.saturating_add(1);
                    self.rejected_events = self.rejected_events.saturating_add(1);
                    break;
                };
                self.queue_bytes = self.queue_bytes.saturating_sub(evicted.payload.len());
                first_evicted.get_or_insert(evicted.message_index);
                last_evicted = Some(evicted.message_index);
                evicted_count = evicted_count.saturating_add(1);
                self.dropped_events = self.dropped_events.saturating_add(1);
            }

            if !inserted {
                continue;
            }
            self.received_events = self.received_events.saturating_add(1);
            accepted += 1;
        }

        EnqueueResult {
            accepted,
            rejected,
            logged,
        }
    }

    fn index_event(&self, event: Vec<u8>, message_index: u64) -> EventRecord {
        let mut value: Value = serde_json::from_slice(&event).unwrap_or(Value::Null);
        match &mut value {
            Value::Object(object) => {
                object.insert(
                    "_zccusan_message_index".to_string(),
                    Value::from(message_index),
                );
            }
            _ => {
                value = serde_json::json!({
                    "_zccusan_message_index": message_index,
                    "event_payload": value,
                });
            }
        }
        EventRecord {
            message_index,
            payload: serde_json::to_vec(&value).unwrap_or(event),
        }
    }

    fn push_record(&mut self, record: EventRecord) {
        self.queue_bytes += record.payload.len();
        self.queue.push_back(record);
    }

    fn snapshot_batch(&self) -> Vec<EventRecord> {
        self.queue.iter().cloned().collect()
    }

    fn acknowledge_through(&mut self, message_index: u64) {
        while self
            .queue
            .front()
            .is_some_and(|record| record.message_index <= message_index)
        {
            if let Some(record) = self.queue.pop_front() {
                self.queue_bytes = self.queue_bytes.saturating_sub(record.payload.len());
            }
        }
    }

    fn metrics(&self) -> String {
        let lines = [
            format!("zccusan_telemetry_events_buffered {}", self.queue.len()),
            format!("zccusan_telemetry_buffer_bytes {}", self.queue_bytes),
            format!(
                "zccusan_telemetry_events_received_total {}",
                self.received_events
            ),
            format!(
                "zccusan_telemetry_events_dropped_total {}",
                self.dropped_events
            ),
            format!(
                "zccusan_telemetry_events_rejected_total {}",
                self.rejected_events
            ),
            format!("zccusan_telemetry_events_sent_total {}", self.sent_events),
            format!(
                "zccusan_telemetry_events_send_failures_total {}",
                self.send_failures
            ),
            format!("zccusan_telemetry_last_send_ms {}", self.last_send_ms),
            format!(
                "zccusan_telemetry_buffer_capacity_bytes {}",
                EVENT_BUFFER_CAPACITY_BYTES
            ),
        ];
        lines.join("\n")
    }
}

fn overflow_event(
    message_index: u64,
    first_evicted: u64,
    last_evicted: u64,
    evicted_count: u64,
) -> EventRecord {
    let payload = serde_json::to_vec(&serde_json::json!({
        "_zccusan_message_index": message_index,
        "event_type": "telemetry_buffer_overflow",
        "event_at_ms": EventServer::current_ms(),
        "evicted_first_message_index": first_evicted,
        "evicted_last_message_index": last_evicted,
        "evicted_count": evicted_count,
        "evicted_events_were_logged_to_stdout": true,
        "stdout_copy_status": "previously_emitted",
        "upstream_acknowledged_before_eviction": false,
        "eviction_reason": "ring_capacity_without_upstream_ack",
    }))
    .unwrap_or_default();
    EventRecord {
        message_index,
        payload,
    }
}

fn spawn_sender_thread(
    state: Arc<SharedState>,
    upstream_url: Option<String>,
    flush_interval_ms: u64,
    request_timeout_ms: u64,
) {
    let Some(upstream_url) = upstream_url else {
        return;
    };

    thread::spawn(move || {
        let client = match Client::builder()
            .timeout(Duration::from_millis(request_timeout_ms))
            .connect_timeout(Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS))
            .pool_max_idle_per_host(0)
            .build()
        {
            Ok(client) => client,
            Err(_) => return,
        };

        let retry_delay = Duration::from_millis(flush_interval_ms);
        let mut next_attempt = Instant::now();

        loop {
            let batch = {
                let (mutex, cvar) = (&state.inner, &state.ready);
                let mut server = lock_mutex(mutex);
                while server.queue.is_empty() {
                    server = match cvar.wait(server) {
                        Ok(guard) => guard,
                        Err(err) => err.into_inner(),
                    };
                }

                while Instant::now() < next_attempt {
                    let delay = next_attempt.saturating_duration_since(Instant::now());
                    server = match cvar.wait_timeout(server, delay) {
                        Ok((guard, _)) => guard,
                        Err(err) => err.into_inner().0,
                    };
                }
                server.snapshot_batch()
            };

            if batch.is_empty() {
                continue;
            }

            let body = serialize_events(&batch);
            let response = client
                .post(&upstream_url)
                .header(CONTENT_TYPE, "application/json")
                .header("Connection", "close")
                .body(body)
                .send();

            match response {
                Ok(resp) if resp.status().is_success() => {
                    let mut server = lock_mutex(&state.inner);
                    if let Some(last) = batch.last() {
                        server.acknowledge_through(last.message_index);
                    }
                    server.sent_events = server.sent_events.saturating_add(batch.len() as u64);
                    server.last_send_ms = EventServer::current_ms();
                }
                Ok(_) | Err(_) => {
                    let mut server = lock_mutex(&state.inner);
                    server.send_failures = server.send_failures.saturating_add(1);
                }
            }
            next_attempt = Instant::now() + retry_delay;
        }
    });
}

fn handle_connection(mut stream: TcpStream, state: Arc<SharedState>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let request = read_http_request(&mut stream)?;

    match (request.method.as_str(), request.path.as_str()) {
        ("POST", EVENTS_PATH) => match parse_event_payload(request.body) {
            Ok(events) => {
                let result = {
                    let mut server = lock_mutex(&state.inner);
                    let result = server.enqueue_events(events);
                    if result.accepted > 0 {
                        state.ready.notify_all();
                    }
                    result
                };
                let response = match result.rejected {
                    0 => format!("{{\"accepted\":{}}}\n", result.accepted).into_bytes(),
                    _ => format!(
                        "{{\"accepted\":{},\"rejected\":{},\"max_event_bytes\":{}}}\n",
                        result.accepted, result.rejected, MAX_EVENT_BYTES
                    )
                    .into_bytes(),
                };

                let (status, reason) = if result.accepted == 0 && result.rejected > 0 {
                    (413, "Payload Too Large")
                } else {
                    (200, "OK")
                };
                write_http_response(&mut stream, status, reason, b"application/json", &response)?;
                let _ = stream.shutdown(Shutdown::Both);
                drop(stream);
                if let Err(err) = log_events_ndjson(&result.logged) {
                    eprintln!("telemetry stdout NDJSON write failed: {err}");
                }
            }
            Err(err) => {
                let response = format!("{{\"error\":\"{}\"}}\n", err).into_bytes();
                write_http_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    b"application/json",
                    &response,
                )?;
            }
        },
        ("GET", METRICS_PATH) => {
            let body = {
                let server = lock_mutex(&state.inner);
                format!("{}\n", server.metrics())
            }
            .into_bytes();
            write_http_response(&mut stream, 200, "OK", b"text/plain; version=0.0.4", &body)?;
        }
        _ => {
            write_http_response(&mut stream, 404, "Not Found", b"text/plain", b"not found\n")?;
        }
    }

    Ok(())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end: Option<usize> = None;

    loop {
        let len = stream.read(&mut chunk)?;
        if len == 0 {
            break;
        }

        raw.extend_from_slice(&chunk[..len]);

        if raw.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }

        if let Some(pos) = find_subsequence(&raw, b"\r\n\r\n") {
            header_end = Some(pos);
            break;
        }
    }

    let Some(pos) = header_end else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed HTTP headers",
        ));
    };

    let headers_blob = std::str::from_utf8(&raw[..pos])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 in headers"))?;

    let mut lines = headers_blob.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;

    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request method"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request path"))?
        .to_string();

    let mut content_length = None::<usize>;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            if let Ok(value) = value.trim().parse::<usize>() {
                content_length = Some(value);
            }
        }
    }

    let mut body = if pos + 4 < raw.len() {
        raw[pos + 4..].to_vec()
    } else {
        Vec::new()
    };

    if method == "POST" {
        if let Some(expected) = content_length {
            if expected > MAX_REQUEST_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request body too large",
                ));
            }

            while body.len() < expected {
                let len = stream.read(&mut chunk)?;
                if len == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..len]);
                if body.len() > MAX_REQUEST_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "request body too large",
                    ));
                }
            }

            if body.len() > expected {
                body.truncate(expected);
            }
        }
    }

    Ok(HttpRequest { method, path, body })
}

fn parse_event_payload(body: Vec<u8>) -> Result<Vec<Vec<u8>>, &'static str> {
    let text = std::str::from_utf8(&body).map_err(|_| "body is not valid UTF-8")?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty body");
    }

    if trimmed.starts_with('[') {
        let values: Vec<Value> = serde_json::from_str(trimmed).map_err(|_| "invalid JSON array")?;
        let mut events = Vec::with_capacity(values.len());
        for value in values {
            events.push(serde_json::to_vec(&value).map_err(|_| "invalid JSON value")?);
        }
        if events.is_empty() {
            return Err("empty array");
        }
        return Ok(events);
    }

    if trimmed.starts_with('{') {
        let single: Value = serde_json::from_str(trimmed).map_err(|_| "invalid JSON event")?;
        return serde_json::to_vec(&single)
            .map(|event| vec![event])
            .map_err(|_| "invalid JSON event");
    }

    let mut events = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|_| "invalid NDJSON line")?;
        events.push(serde_json::to_vec(&value).map_err(|_| "invalid NDJSON line")?);
    }

    if events.is_empty() {
        return Err("empty events stream");
    }

    Ok(events)
}

fn log_events_ndjson(events: &[Vec<u8>]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    write_events_ndjson(&mut output, events)
}

fn write_events_ndjson(output: &mut impl Write, events: &[Vec<u8>]) -> io::Result<()> {
    for event in events {
        if event.is_empty() || event.len() > EVENT_BUFFER_CAPACITY_BYTES {
            continue;
        }
        output.write_all(event)?;
        output.write_all(b"\n")?;
    }
    output.flush()
}

fn serialize_events(events: &[EventRecord]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(b'[');
    for (i, event) in events.iter().enumerate() {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(&event.payload);
    }
    body.push(b']');
    body
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &[u8],
    body: &[u8],
) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        std::str::from_utf8(content_type).unwrap_or("text/plain"),
        body.len(),
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)
}

fn read_http_url(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| is_http_url(value))
}

fn select_upstream(
    survey_enabled: bool,
    telemetry_upstream: Option<String>,
    survey_backend: Option<String>,
) -> Option<String> {
    survey_enabled
        .then_some(telemetry_upstream.or(survey_backend))
        .flatten()
}

fn env_enabled(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" => Some(true),
            "0" | "false" | "no" | "off" | "disabled" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }

    (0..=haystack.len() - needle.len())
        .find(|&start| haystack[start..start + needle.len()] == *needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_events_are_written_as_one_line_ndjson() {
        let events = parse_event_payload(
            br#"[{"event":"startup","node":"a"},{"event":"snapshot","ok":true}]"#.to_vec(),
        )
        .expect("valid JSON events");
        let mut output = Vec::new();

        write_events_ndjson(&mut output, &events).expect("write NDJSON");

        assert_eq!(
            output,
            b"{\"event\":\"startup\",\"node\":\"a\"}\n{\"event\":\"snapshot\",\"ok\":true}\n"
        );
    }

    #[test]
    fn explicit_upstream_precedes_survey_backend() {
        assert_eq!(
            select_upstream(
                true,
                Some("https://collector.example/events".to_string()),
                Some("https://survey.example/survey".to_string()),
            ),
            Some("https://collector.example/events".to_string())
        );
    }

    #[test]
    fn survey_opt_out_disables_outbound_forwarding_only() {
        assert_eq!(
            select_upstream(
                false,
                Some("https://collector.example/events".to_string()),
                Some("https://survey.example/survey".to_string()),
            ),
            None
        );
    }

    #[test]
    fn events_over_four_kibibytes_are_rejected() {
        let mut server = EventServer::new();
        let result = server.enqueue_events(vec![vec![b'x'; MAX_EVENT_BYTES + 1]]);

        assert_eq!(result.accepted, 0);
        assert_eq!(result.rejected, 1);
        assert!(result.logged.is_empty());
        assert!(server.queue.is_empty());
    }

    #[test]
    fn ring_eviction_is_indexed_logged_and_precedes_the_new_event() {
        let mut server = EventServer::new();
        let event = serde_json::to_vec(&serde_json::json!({
            "event_type": "capacity_test",
            "payload": "x".repeat(3_900),
        }))
        .expect("serialize test event");
        assert!(event.len() <= MAX_EVENT_BYTES);

        let mut overflow = None;
        for _ in 0..2_000 {
            let result = server.enqueue_events(vec![event.clone()]);
            if result.logged.len() == 2 {
                overflow = Some(result.logged);
                break;
            }
        }

        let logged = overflow.expect("ring should eventually evict an old event");
        let overflow: Value = serde_json::from_slice(&logged[0]).expect("overflow JSON");
        let appended: Value = serde_json::from_slice(&logged[1]).expect("appended event JSON");
        assert_eq!(
            overflow["event_type"],
            Value::String("telemetry_buffer_overflow".to_string())
        );
        assert!(overflow["evicted_first_message_index"].as_u64().unwrap() > 0);
        assert!(
            overflow["evicted_last_message_index"].as_u64().unwrap()
                >= overflow["evicted_first_message_index"].as_u64().unwrap()
        );
        assert!(overflow["evicted_count"].as_u64().unwrap() > 0);
        assert_eq!(overflow["evicted_events_were_logged_to_stdout"], true);
        assert!(
            overflow["_zccusan_message_index"].as_u64().unwrap()
                < appended["_zccusan_message_index"].as_u64().unwrap()
        );
        assert!(server.queue_bytes <= EVENT_BUFFER_CAPACITY_BYTES);
        assert!(server.dropped_events > 0);
    }

    #[test]
    fn snapshots_do_not_remove_unacknowledged_events() {
        let mut server = EventServer::new();
        let result = server.enqueue_events(vec![br#"{"event_type":"retry_test"}"#.to_vec()]);
        assert_eq!(result.accepted, 1);

        let first_attempt = server.snapshot_batch();
        let retry_attempt = server.snapshot_batch();
        assert_eq!(
            first_attempt[0].message_index,
            retry_attempt[0].message_index
        );
        assert_eq!(server.queue.len(), 1);

        server.acknowledge_through(first_attempt[0].message_index);
        assert!(server.queue.is_empty());
    }
}
