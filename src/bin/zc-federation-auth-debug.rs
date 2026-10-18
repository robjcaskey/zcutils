//! Private JSON-lines driver for the loopback HTTP authorization laboratory.
//! Uses the actual placement, failover and lane-limiter implementations. Replica
//! observations and workload-adapter acknowledgements are explicitly modeled;
//! this executable does not claim to be a Raft or durable HA service.

use aws_lc_rs::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use zcutils::{
    federation_auth::{AuthorizationPolicy, VerifiedIdentity, denied},
    global_failover::*,
    global_policy::*,
    iops_policy::{LaneBudgetSnapshot, LaneLimiter},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    authorization: AuthorizationPolicy,
    data_dir: PathBuf,
    west_owned: BTreeSet<String>,
    atomic_groups: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    token: String,
    region: String,
    action: String,
    resource: String,
    #[serde(default)]
    approval: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    target_region: Option<String>,
    #[serde(default)]
    record_key: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    epoch: Option<u64>,
    #[serde(default)]
    ops: Option<u32>,
    #[serde(default)]
    replica: Option<Replica>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Data {
    sequence: u64,
    records: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replica {
    resource: String,
    source: String,
    target: String,
    epoch: u64,
    nonce: String,
    ciphertext: String,
}

struct Lab {
    policy: AuthorizationPolicy,
    global: GlobalPolicyState,
    data: BTreeMap<(String, String), Data>,
    keys: BTreeMap<String, [u8; 32]>,
    budgets: BTreeMap<String, LaneLimiter>,
    data_dir: PathBuf,
    clock: Instant,
    operation: u64,
    tenant_metadata: BTreeMap<(String, Vec<String>), String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}
fn bad(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason.to_owned())
}
fn region(value: &str) -> io::Result<()> {
    if ["us-east", "us-west"].contains(&value) {
        Ok(())
    } else {
        Err(denied("unknown_region"))
    }
}
fn id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
}

impl Lab {
    fn new(config: Config) -> io::Result<Self> {
        config.authorization.validate()?;
        let mut lab = Self {
            policy: config.authorization,
            global: GlobalPolicyState::default(),
            data: BTreeMap::new(),
            keys: BTreeMap::new(),
            budgets: BTreeMap::new(),
            data_dir: config.data_dir,
            clock: Instant::now(),
            operation: 0,
            tenant_metadata: BTreeMap::new(),
        };
        fs::create_dir_all(&lab.data_dir)?;
        if fs::read_dir(&lab.data_dir)?.next().is_some() {
            return Err(bad("debug_data_directory_must_be_empty"));
        }
        for (name, resource) in &lab.policy.resources {
            if name != &resource.id
                || !id(name)
                || !id(&resource.budget_id)
                || resource.tenant.is_empty()
                || !resource.tenant.iter().all(|s| id(s))
            {
                return Err(bad("invalid_resource_catalog"));
            }
            let owner = if config.west_owned.contains(name) {
                "us-west"
            } else {
                "us-east"
            };
            lab.global.failover.put_volume(
                lab.global.failover.revision,
                VolumeSpec {
                    volume_id: name.clone(),
                    authority_region: owner.into(),
                    placement_epoch: 1,
                    consistency_set_id: None,
                    workload_bindings: vec![WorkloadBinding {
                        binding_id: format!("workload-{name}"),
                        adapter_id: "debug-external-adapter".into(),
                        adapter_kind: AdapterKind::External,
                        policy: WorkloadFailoverPolicy::FollowVolume,
                        source_replicas: 1,
                        target_replicas: 1,
                    }],
                },
            )?;
            let mut key = [0; 32];
            SystemRandom::new()
                .fill(&mut key)
                .map_err(|_| bad("random_key_failed"))?;
            lab.keys.insert(name.clone(), key);
            lab.data.insert(
                (owner.into(), name.clone()),
                Data {
                    sequence: 1,
                    records: BTreeMap::from([("seed".into(), format!("private-{name}"))]),
                },
            );
            let rate = if name == "quota-probe" { 1 } else { 100_000 };
            let quantum = if name == "quota-probe" { 4 } else { 256 };
            lab.budgets
                .entry(resource.budget_id.clone())
                .or_insert_with(|| {
                    LaneLimiter::new(
                        0,
                        LaneBudgetSnapshot {
                            generation: 1,
                            sustained_iops: rate,
                            peak_iops: rate,
                            burst_ops: quantum,
                            quantum_ops: quantum,
                            metric_publish_ns: 100_000_000,
                            effective_ns: 0,
                            fallback_sustained_iops: rate,
                            fallback_peak_iops: rate,
                            valid_until_ns: 0,
                        },
                    )
                });
        }
        let mut grouped = BTreeSet::new();
        for (set, members) in config.atomic_groups {
            grouped.extend(members.iter().cloned());
            lab.global.failover.put_consistency_set(
                lab.global.failover.revision,
                ConsistencySetSpec {
                    set_id: set,
                    members,
                    dependencies: vec![],
                },
            )?;
        }
        for name in lab
            .policy
            .resources
            .keys()
            .filter(|n| !grouped.contains(*n))
        {
            lab.global.failover.put_consistency_set(
                lab.global.failover.revision,
                ConsistencySetSpec {
                    set_id: format!("set-{name}"),
                    members: BTreeSet::from([name.clone()]),
                    dependencies: vec![],
                },
            )?;
        }
        for spec in lab
            .global
            .failover
            .volumes
            .values()
            .cloned()
            .collect::<Vec<_>>()
        {
            lab.global.failover.register_session(ClientSession {
                session_id: format!("session-{}", spec.volume_id),
                volume_id: spec.volume_id,
                frontend: "debug-http-workload".into(),
                region: spec.authority_region,
                observed_placement_epoch: 1,
                supports_transparent_rebind: true,
                state: SessionState::Active,
                fence_reason: None,
            })?;
        }
        // Real bilateral policy checks, separate from identity authentication.
        for (source, target) in [("us-east", "us-west"), ("us-west", "us-east")] {
            let grant = RegionalTrustGrant {
                grant_id: format!("{source}-to-{target}"),
                owner_region_id: source.into(),
                delegate_region_id: target.into(),
                generation: 1,
                permissions: RegionalTrustPermissions {
                    store_encrypted_replicas: true,
                    serve_encrypted_restore: true,
                    ..Default::default()
                },
            };
            lab.commit(GlobalPolicyCommand::SetRegionTrust { grant })?;
            lab.commit(GlobalPolicyCommand::SetRegionalInboundPolicy {
                policy: RegionalInboundPolicy {
                    region_id: target.into(),
                    generation: 1,
                    allowed_source_regions: BTreeSet::from([source.into()]),
                    accept_encrypted_volumes: true,
                    accept_unencrypted_volumes: false,
                    accept_key_escrow: KeyEscrowMode::Denied,
                    max_volume_bytes: 1_048_576,
                    allowed_data_classes: BTreeSet::from(["debug-tenant".into()]),
                    required_attributes: BTreeMap::new(),
                    denied_attributes: BTreeMap::new(),
                },
            })?;
        }
        for (r, v) in lab.data.keys().cloned().collect::<Vec<_>>() {
            lab.persist(&r, &v)?;
        }
        Ok(lab)
    }

    fn commit(&mut self, command: GlobalPolicyCommand) -> io::Result<()> {
        self.global
            .apply(self.global.applied_index + 1, 1, &command)
    }
    fn check(
        &self,
        identity: &VerifiedIdentity,
        req: &Request,
        region: &str,
        resource: &str,
        action: &str,
    ) -> io::Result<()> {
        self.policy
            .authorize(
                identity,
                region,
                resource,
                action,
                req.approval.as_deref(),
                req.status.as_deref(),
                now(),
            )
            .map(|_| ())
    }
    fn custody(&self, req: &Request) -> io::Result<()> {
        let spec = self
            .global
            .failover
            .volumes
            .get(&req.resource)
            .ok_or_else(|| denied("resource_denied"))?;
        if spec.authority_region != req.region {
            return Err(denied("region_not_authoritative"));
        }
        if req.epoch != Some(spec.placement_epoch) {
            return Err(denied("stale_placement_epoch"));
        }
        Ok(())
    }
    fn charge(&mut self, resource: &str, ops: u32) -> io::Result<u32> {
        if ops == 0 || ops > 256 {
            return Err(bad("invalid_batch_size"));
        }
        let budget = &self.policy.resources[resource].budget_id;
        let limiter = self.budgets.get_mut(budget).expect("catalog budget");
        let admitted = limiter.admit(self.clock.elapsed().as_nanos() as u64, ops);
        if admitted == 0 {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "budget_exhausted",
            ))
        } else {
            Ok(admitted)
        }
    }
    fn seal(&self, source: &str, target: &str, resource: &str) -> io::Result<Replica> {
        let mut replica = Replica {
            resource: resource.into(),
            source: source.into(),
            target: target.into(),
            epoch: self.global.failover.volumes[resource].placement_epoch,
            nonce: String::new(),
            ciphertext: String::new(),
        };
        let aad = serde_json::to_vec(&(
            &self.policy.federation,
            &replica.resource,
            &replica.source,
            &replica.target,
            replica.epoch,
        ))?;
        let mut nonce = [0; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| bad("random_nonce_failed"))?;
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_256_GCM, &self.keys[resource])
                .map_err(|_| bad("key_failed"))?,
        );
        let mut bytes = serde_json::to_vec(&self.data[&(source.into(), resource.into())])?;
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad),
            &mut bytes,
        )
        .map_err(|_| bad("encryption_failed"))?;
        replica.nonce = B64.encode(nonce);
        replica.ciphertext = B64.encode(bytes);
        Ok(replica)
    }
    fn open(&self, replica: &Replica) -> io::Result<Data> {
        let aad = serde_json::to_vec(&(
            &self.policy.federation,
            &replica.resource,
            &replica.source,
            &replica.target,
            replica.epoch,
        ))?;
        let nonce: [u8; 12] = B64
            .decode(&replica.nonce)
            .map_err(|_| bad("bad_nonce"))?
            .try_into()
            .map_err(|_| bad("bad_nonce"))?;
        let mut bytes = B64
            .decode(&replica.ciphertext)
            .map_err(|_| bad("bad_ciphertext"))?;
        let secret = self
            .keys
            .get(&replica.resource)
            .ok_or_else(|| denied("resource_denied"))?;
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_256_GCM, secret).map_err(|_| bad("key_failed"))?,
        );
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad),
                &mut bytes,
            )
            .map_err(|_| denied("replica_authentication_failed"))?;
        serde_json::from_slice(plain).map_err(|_| bad("invalid_replica_data"))
    }
    fn persist(&self, region: &str, resource: &str) -> io::Result<()> {
        let path = self.data_dir.join(format!("{region}-{resource}.json"));
        let tmp = path.with_extension("tmp");
        fs::write(
            &tmp,
            serde_json::to_vec(&self.seal(region, region, resource)?)?,
        )?;
        fs::rename(tmp, path)
    }
    fn placement(&self, source: &str, target: &str) -> io::Result<()> {
        self.global
            .authorize_volume_placement(
                &format!("{source}-to-{target}"),
                &VolumePlacementRequest {
                    source_region_id: source.into(),
                    destination_region_id: target.into(),
                    encryption: VolumeEncryption::Encrypted,
                    volume_bytes: 4096,
                    data_class: "debug-tenant".into(),
                    attributes: BTreeMap::new(),
                },
                true,
            )
            .map_err(|_| denied("bilateral_placement_denied"))
    }
    fn handle(&mut self, req: Request) -> io::Result<Value> {
        region(&req.region)?;
        let identity = self.policy.identity(&req.token, now())?;
        let action = match req.action.as_str() {
            "export" | "import" => "replicate",
            other => other,
        };
        self.check(&identity, &req, &req.region, &req.resource, action)?;
        match req.action.as_str() {
            "read" => {
                let data = self
                    .data
                    .get(&(req.region.clone(), req.resource.clone()))
                    .ok_or_else(|| bad("replica_missing"))?;
                Ok(
                    json!({"data":data,"volume":self.global.failover.volumes[&req.resource],"session":self.global.failover.sessions[&format!("session-{}",req.resource)],"budget_id":self.policy.resources[&req.resource].budget_id,"tenant_label":self.tenant_metadata.get(&(req.region.clone(),self.policy.resources[&req.resource].tenant.clone()))}),
                )
            }
            "write" | "delete_record" => {
                self.custody(&req)?;
                let key = req
                    .record_key
                    .as_deref()
                    .filter(|s| !s.is_empty() && s.len() <= 128)
                    .ok_or_else(|| bad("record_key_required"))?;
                let value = if req.action == "write" {
                    Some(
                        req.value
                            .as_deref()
                            .filter(|s| s.len() <= 4096)
                            .ok_or_else(|| bad("value_required"))?,
                    )
                } else {
                    None
                };
                let current = &self.data[&(req.region.clone(), req.resource.clone())];
                if value.is_some()
                    && !current.records.contains_key(key)
                    && current.records.len() >= 256
                {
                    return Err(bad("debug_record_limit"));
                }
                self.charge(&req.resource, 1)?;
                let data = self
                    .data
                    .get_mut(&(req.region.clone(), req.resource.clone()))
                    .expect("custody has data");
                if let Some(value) = value {
                    data.records.insert(key.into(), value.into());
                } else {
                    data.records.remove(key);
                }
                data.sequence += 1;
                self.persist(&req.region, &req.resource)?;
                Ok(json!({"sequence":self.data[&(req.region,req.resource)].sequence}))
            }
            "export" => {
                self.custody(&req)?;
                let target = req
                    .target_region
                    .as_deref()
                    .ok_or_else(|| bad("target_required"))?;
                region(target)?;
                self.check(&identity, &req, target, &req.resource, "replicate")?;
                self.placement(&req.region, target)?;
                Ok(json!({"replica":self.seal(&req.region,target,&req.resource)?}))
            }
            "import" => {
                let replica = req
                    .replica
                    .as_ref()
                    .ok_or_else(|| bad("replica_required"))?;
                region(&replica.source)?;
                if replica.resource != req.resource
                    || replica.target != req.region
                    || replica.source == replica.target
                {
                    return Err(denied("replica_binding_mismatch"));
                }
                self.check(&identity, &req, &replica.source, &req.resource, "replicate")?;
                self.placement(&replica.source, &req.region)?;
                let volume = &self.global.failover.volumes[&req.resource];
                if volume.authority_region != replica.source
                    || volume.placement_epoch != replica.epoch
                {
                    return Err(denied("stale_replica_epoch"));
                }
                let data = self.open(replica)?;
                if let Some(existing) = self.data.get(&(req.region.clone(), req.resource.clone())) {
                    if data.sequence < existing.sequence
                        || (data.sequence == existing.sequence && &data != existing)
                    {
                        return Err(denied("stale_or_conflicting_replica"));
                    }
                }
                self.data
                    .insert((req.region.clone(), req.resource.clone()), data);
                self.persist(&req.region, &req.resource)?;
                Ok(json!({"replicated":true}))
            }
            "manage_tenant" => {
                // Mutable tenant metadata, without ownership/grant/budget edits.
                let label = req
                    .value
                    .as_deref()
                    .filter(|s| !s.is_empty() && s.len() <= 128)
                    .ok_or_else(|| bad("tenant_label_required"))?;
                let tenant = self.policy.resources[&req.resource].tenant.clone();
                let result = json!({"managed_tenant":tenant,"label":label,"budget_id":self.policy.resources[&req.resource].budget_id});
                fs::write(
                    self.data_dir
                        .join(format!("{}-{}-tenant.json", req.region, req.resource)),
                    serde_json::to_vec(&result)?,
                )?;
                self.tenant_metadata
                    .insert((req.region.clone(), tenant), label.into());
                Ok(result)
            }
            "quota" => {
                let admitted = self.charge(&req.resource, req.ops.unwrap_or(1))?;
                Ok(
                    json!({"admitted":admitted,"budget_id":self.policy.resources[&req.resource].budget_id}),
                )
            }
            "failover" => self.failover(&identity, &req),
            _ => Err(denied("unsupported_action")),
        }
    }
    fn failover(&mut self, identity: &VerifiedIdentity, req: &Request) -> io::Result<Value> {
        self.custody(req)?;
        let target = req
            .target_region
            .as_deref()
            .ok_or_else(|| bad("target_required"))?;
        region(target)?;
        let volume = &self.global.failover.volumes[&req.resource];
        let set_id = volume
            .consistency_set_id
            .as_ref()
            .expect("configured set")
            .clone();
        let members = self.global.failover.consistency_sets[&set_id]
            .members
            .clone();
        // Expand and authorize the complete atomic closure BEFORE any state
        // mutation, checkpoint, replica observation, or workload side effect.
        for member in &members {
            self.check(identity, req, &req.region, member, "failover")?;
            self.check(identity, req, target, member, "failover")?;
            self.placement(&req.region, target)?;
            if self.data.get(&(req.region.clone(), member.clone()))
                != self.data.get(&(target.into(), member.clone()))
            {
                return Err(denied("replica_not_caught_up"));
            }
        }
        let mut state = self.global.failover.clone();
        let revision = state.revision;
        let mut cuts = BTreeMap::new();
        for member in &members {
            let hwm = self.data[&(req.region.clone(), member.clone())].sequence;
            cuts.insert(member.clone(), BTreeMap::from([(0, hwm)]));
            for r in [&req.region, &target.to_string()] {
                state.observe_region(RegionalVolumeProgress {
                    volume_id: member.clone(),
                    region: r.clone(),
                    observed_revision: revision,
                    placement_epoch: state.volumes[member].placement_epoch,
                    durable_lane_hwms: BTreeMap::from([(0, hwm)]),
                    applied_lane_hwms: BTreeMap::from([(0, hwm)]),
                    configured_replicas: 3,
                    required_quorum: 2,
                    live_replicas: 3,
                    durable_failure_domains: BTreeSet::from([
                        "modeled-a".into(),
                        "modeled-b".into(),
                        "modeled-c".into(),
                    ]),
                    reachable: true,
                    quiesced: r == &req.region,
                })?;
            }
        }
        self.operation += 1;
        let operation_id = format!("authorized-cut-{}", self.operation);
        state.publish_checkpoint(ConsistencyCheckpoint {
            checkpoint_id: format!("checkpoint-{}", self.operation),
            set_id,
            sequence: self.operation,
            cuts,
            application_consistent: true,
        })?;
        state.request_failover(FailoverRequest {
            operation_id: operation_id.clone(),
            requested_volume_ids: BTreeSet::from([req.resource.clone()]),
            source_region: req.region.clone(),
            target_region: target.into(),
            expected_revision: revision,
            mode: FailoverMode::Clean,
        })?;
        state.reconcile(&operation_id)?;
        let actions: Vec<_> = state
            .workload_actions
            .values()
            .filter(|a| a.operation_id == operation_id)
            .cloned()
            .collect();
        for a in &actions {
            state.acknowledge_workload_action(&a.action_id, &a.adapter_id)?;
        }
        let phase = state.reconcile(&operation_id)?;
        if phase != FailoverPhase::Completed {
            return Err(bad("failover_did_not_complete"));
        }
        // Recheck authorization at the simulated commit, including hard expiry.
        for member in &members {
            self.check(identity, req, &req.region, member, "failover")?;
            self.check(identity, req, target, member, "failover")?;
        }
        self.global.failover = state;
        let actions: Vec<_> = actions
            .iter()
            .map(|a| &self.global.failover.workload_actions[&a.action_id])
            .collect();
        Ok(
            json!({"operation":self.global.failover.operations[&operation_id],"workload_actions":actions,"volumes":members.iter().map(|m|&self.global.failover.volumes[m]).collect::<Vec<_>>() }),
        )
    }
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || args[1] != "--debug-config" {
        return Err(bad(
            "usage: zc-federation-auth-debug --debug-config FILE (private stdin JSONL; debug only)",
        ));
    }
    let config: Config = serde_json::from_slice(&fs::read(&args[2])?)?;
    let mut lab = Lab::new(config)?;
    eprintln!(
        "ZC_AUTH_DEBUG topology=two-loopback-regions consensus=in-process-model ha-observations=modeled representative_benchmark=false"
    );
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let response = match line
            .and_then(|l| {
                if l.len() > 65_536 {
                    Err(bad("request_too_large"))
                } else {
                    serde_json::from_str::<Request>(&l).map_err(|_| bad("invalid_request"))
                }
            })
            .and_then(|r| lab.handle(r))
        {
            Ok(result) => json!({"ok":true,"result":result}),
            Err(e) => {
                json!({"ok":false,"error":e.to_string(),"status":if e.kind()==io::ErrorKind::PermissionDenied {403} else if e.kind()==io::ErrorKind::WouldBlock {429} else {400}})
            }
        };
        writeln!(out, "{response}")?;
        out.flush()?;
    }
    Ok(())
}
