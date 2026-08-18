//! Permissionless cloud topology discovery.
//!
//! EC2 discovery uses only IMDSv2 from the local instance. Missing optional
//! metadata remains absent; callers can add or override arbitrary facts.

use crate::topology::Characteristics;
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub trait MetadataSource {
    fn get(&self, path: &str) -> io::Result<Option<String>>;
}

pub struct Ec2Imds {
    address: SocketAddr,
    timeout: Duration,
    token: String,
}

impl Ec2Imds {
    pub fn connect() -> io::Result<Self> {
        let address = "169.254.169.254:80"
            .parse()
            .expect("constant IMDS socket address");
        let timeout = Duration::from_millis(250);
        let response = http_request(
            address,
            timeout,
            "PUT",
            "/latest/api/token",
            &[("X-aws-ec2-metadata-token-ttl-seconds", "60")],
        )?;
        if response.status != 200 || response.body.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("IMDSv2 token request returned {}", response.status),
            ));
        }
        Ok(Self {
            address,
            timeout,
            token: response.body,
        })
    }
}

impl MetadataSource for Ec2Imds {
    fn get(&self, path: &str) -> io::Result<Option<String>> {
        let path = format!("/latest/{path}");
        let response = http_request(
            self.address,
            self.timeout,
            "GET",
            &path,
            &[("X-aws-ec2-metadata-token", &self.token)],
        )?;
        match response.status {
            200 => Ok(Some(response.body)),
            404 => Ok(None),
            status => Err(io::Error::other(format!(
                "IMDS request {path} returned {status}"
            ))),
        }
    }
}

pub fn detect_ec2(source: &impl MetadataSource) -> io::Result<Characteristics> {
    let mut facts = Characteristics::new();
    let document = source
        .get("dynamic/instance-identity/document")?
        .and_then(|body| serde_json::from_str::<Value>(&body).ok());
    let az = optional(source, "meta-data/placement/availability-zone")?.or_else(|| {
        document
            .as_ref()?
            .get("availabilityZone")?
            .as_str()
            .map(str::to_string)
    });
    let region = document
        .as_ref()
        .and_then(|value| value.get("region"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| az.as_deref().and_then(region_from_az).map(str::to_string));

    facts.insert("cloud.provider".into(), json!("aws"));
    insert_optional(&mut facts, "cloud.region", region.clone());
    insert_optional(&mut facts, "cloud.availability_zone", az.clone());
    insert_optional(
        &mut facts,
        "cloud.availability_zone_id",
        optional(source, "meta-data/placement/availability-zone-id")?,
    );
    insert_optional(
        &mut facts,
        "cloud.placement_group",
        optional(source, "meta-data/placement/group-name")?,
    );
    insert_optional(
        &mut facts,
        "cloud.instance_id",
        optional(source, "meta-data/instance-id")?,
    );
    insert_optional(
        &mut facts,
        "cloud.instance_type",
        optional(source, "meta-data/instance-type")?,
    );
    insert_optional(
        &mut facts,
        "network.local_ipv4",
        optional(source, "meta-data/local-ipv4")?,
    );
    if let Some(region) = region {
        facts.insert("failure.region".into(), json!(region));
    }
    if let Some(az) = az {
        facts.insert("failure.az".into(), json!(az));
    }
    Ok(facts)
}

pub fn apply_overrides(
    facts: &mut Characteristics,
    overrides: impl IntoIterator<Item = String>,
) -> io::Result<()> {
    for override_value in overrides {
        let (key, raw) = override_value.split_once('=').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("topology override must be key=value: {override_value}"),
            )
        })?;
        if key.is_empty() || key.contains('\0') || key.contains('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid topology characteristic key",
            ));
        }
        let value = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
        if value.is_null() {
            facts.remove(key);
        } else {
            facts.insert(key.to_string(), value);
        }
    }
    Ok(())
}

fn optional(source: &impl MetadataSource, path: &str) -> io::Result<Option<String>> {
    Ok(source
        .get(path)?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

fn insert_optional(facts: &mut Characteristics, key: &str, value: Option<String>) {
    if let Some(value) = value {
        facts.insert(key.to_string(), json!(value));
    }
}

fn region_from_az(az: &str) -> Option<&str> {
    let last = az.as_bytes().last()?;
    last.is_ascii_alphabetic()
        .then_some(&az[..az.len().saturating_sub(1)])
}

struct HttpResponse {
    status: u16,
    body: String,
}

fn http_request(
    address: SocketAddr,
    timeout: Duration,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 169.254.169.254\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Content-Length: 0\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let response = String::from_utf8(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .or_else(|| response.split_once("\n\n"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid IMDS HTTP response"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid IMDS status"))?;
    Ok(HttpResponse {
        status,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fake(BTreeMap<String, String>);

    impl MetadataSource for Fake {
        fn get(&self, path: &str) -> io::Result<Option<String>> {
            Ok(self.0.get(path).cloned())
        }
    }

    #[test]
    fn ec2_detection_is_permissionless_and_overridable() {
        let fake = Fake(BTreeMap::from([
            (
                "dynamic/instance-identity/document".into(),
                r#"{"region":"us-east-2","availabilityZone":"us-east-2c"}"#.into(),
            ),
            (
                "meta-data/placement/group-name".into(),
                "tier-cluster".into(),
            ),
            ("meta-data/instance-id".into(), "i-test".into()),
        ]));
        let mut facts = detect_ec2(&fake).unwrap();
        apply_overrides(
            &mut facts,
            [
                "failure.rack=\"rack-7\"".into(),
                "cloud.region=\"manual\"".into(),
            ],
        )
        .unwrap();
        assert_eq!(facts["cloud.availability_zone"], "us-east-2c");
        assert_eq!(facts["cloud.placement_group"], "tier-cluster");
        assert_eq!(facts["failure.rack"], "rack-7");
        assert_eq!(facts["cloud.region"], "manual");
    }
}
