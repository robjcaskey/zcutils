//! Portable, content-addressed kernel-module bundle and catalog contracts.
//!
//! Source URLs and regional trust policy intentionally do not appear here.
//! A region chooses its own trusted artifact sources, then resolves immutable
//! catalog and bundle objects through those sources.  Nodes must still inspect
//! the downloaded module and let the kernel verify its embedded signature when
//! loading it; catalog claims are not treated as observed facts.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const API_GROUP: &str = "artifacts.zcutils.io";
pub const API_VERSION: &str = "v1alpha1";

fn default_true() -> bool {
    true
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "artifacts.zcutils.io",
    version = "v1alpha1",
    kind = "ZccusanKernelModuleSource",
    plural = "zccusankernelmodulesources",
    shortname = "zckms",
    status = "ZccusanKernelModuleSourceStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct ZccusanKernelModuleSourceSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Only nodes matching these labels may use this source policy. An empty
    /// selector applies cluster-wide, which is convenient for a single-region
    /// cluster but should be used deliberately in a shared regional cluster.
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
    /// Ordered static origins. Hints order equivalent origins; they do not
    /// widen the node selector or confer trust.
    pub endpoints: Vec<KernelModuleSourceEndpoint>,
    /// Signed catalogs accepted through these origins.
    pub catalog_refs: Vec<KernelModuleCatalogReference>,
    /// Public verification keys. These are not secret data, but ConfigMaps
    /// allow a regional administrator to rotate them without rebuilding Pods.
    pub trusted_public_key_refs: Vec<PublicKeyConfigMapReference>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleSourceEndpoint {
    /// User-provided static HTTP(S) origin. There is intentionally no default
    /// origin owned by the project or any cloud provider.
    pub base_url: String,
    #[serde(default)]
    pub allow_insecure_http: bool,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub topology_hints: KernelModuleSourceTopologyHints,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleSourceTopologyHints {
    #[serde(default)]
    pub cloud_providers: Vec<String>,
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default)]
    pub availability_zones: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleCatalogReference {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicKeyConfigMapReference {
    pub namespace: String,
    pub name: String,
    pub key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ZccusanKernelModuleSourceStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "artifacts.zcutils.io",
    version = "v1alpha1",
    kind = "ZccusanKernelModuleBundle",
    plural = "zccusankernelmodulebundles",
    shortname = "zckmb",
    status = "ZccusanKernelModuleBundleStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct ZccusanKernelModuleBundleSpec {
    /// The immutable canonical manifest from which this CR was materialized.
    pub manifest: ContentAddressedArtifact,
    pub module: KernelModuleArtifact,
    pub compatibility: KernelModuleCompatibility,
    pub build: KernelModuleBuildProvenance,
    /// Detached signatures over the canonical manifest bytes. Signer identity
    /// is intentionally not a manifest claim; it is established by verifying
    /// one of these signatures against keys trusted by the consuming region.
    pub signatures: Vec<ArtifactSignature>,
    #[serde(default)]
    pub attestations: Vec<NamedArtifact>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleArtifact {
    pub name: String,
    pub object: ContentAddressedArtifact,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContentAddressedArtifact {
    pub media_type: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// Relative immutable key. Its final path component must be `sha256`.
    /// Regional source base URLs are configured separately.
    pub object_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSignature {
    /// Signature envelope format, for example `Dsse` or `Cms`.
    pub format: String,
    pub object: ContentAddressedArtifact,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NamedArtifact {
    pub name: String,
    pub object: ContentAddressedArtifact,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleCompatibility {
    /// OCI architecture spelling, such as `amd64` or `arm64`.
    pub architecture: String,
    /// Exact `uname -r` / first vermagic field.
    pub kernel_release: String,
    /// Exact module vermagic string observed in the ELF `.modinfo` section.
    pub vermagic: String,
    #[serde(default)]
    pub kernel_build_id: Option<String>,
    pub interfaces: KernelModuleInterfaces,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleInterfaces {
    pub zcnblk_client_edge_abi: u32,
    pub zcnblk_shared_memory_abi: u32,
    pub minimum_userspace_protocol: u32,
    pub maximum_userspace_protocol: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleBuildProvenance {
    pub source_revision: String,
    pub source_tree_sha256: String,
    pub build_recipe_sha256: String,
    /// An immutable OCI digest, not a mutable tag.
    pub toolchain_image_digest: String,
    pub kernel_headers_sha256: String,
    pub kernel_config_sha256: String,
    pub module_symvers_sha256: String,
    pub built_at: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ZccusanKernelModuleBundleStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub accepted_manifest_sha256: Option<String>,
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "artifacts.zcutils.io",
    version = "v1alpha1",
    kind = "ZccusanKernelModuleCatalog",
    plural = "zccusankernelmodulecatalogs",
    shortname = "zckmc",
    status = "ZccusanKernelModuleCatalogStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct ZccusanKernelModuleCatalogSpec {
    /// Monotonically increasing generation inside the signed catalog payload.
    pub catalog_generation: u64,
    pub catalog: ContentAddressedArtifact,
    pub entries: Vec<KernelModuleCatalogEntry>,
    pub signatures: Vec<ArtifactSignature>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KernelModuleCatalogEntry {
    pub module_name: String,
    pub architecture: String,
    pub kernel_release: String,
    #[serde(default)]
    pub kernel_build_id: Option<String>,
    pub interfaces: KernelModuleInterfaces,
    /// Immutable canonical bundle-manifest object, not a regional URL.
    pub manifest: ContentAddressedArtifact,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ZccusanKernelModuleCatalogStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub accepted_catalog_sha256: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObservedKernelModule {
    pub module_name: String,
    pub architecture: String,
    pub kernel_release: String,
    pub vermagic: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// Presence is only an observation. Trust is established by the kernel's
    /// signature verification policy when it loads the module.
    pub embedded_signature_present: bool,
}

pub fn validate_source_spec(spec: &ZccusanKernelModuleSourceSpec) -> Result<(), String> {
    if spec.endpoints.is_empty() {
        return Err("endpoints must not be empty".into());
    }
    if spec.catalog_refs.is_empty() {
        return Err("catalogRefs must not be empty".into());
    }
    if spec.trusted_public_key_refs.is_empty() {
        return Err("trustedPublicKeyRefs must not be empty".into());
    }
    let mut urls = BTreeMap::<String, usize>::new();
    for (index, endpoint) in spec.endpoints.iter().enumerate() {
        validate_source_url(index, endpoint)?;
        if let Some(previous) = urls.insert(endpoint.base_url.clone(), index) {
            return Err(format!(
                "endpoints[{index}].baseUrl duplicates endpoints[{previous}].baseUrl"
            ));
        }
        for (field, hints) in [
            (
                "cloudProviders",
                endpoint.topology_hints.cloud_providers.as_slice(),
            ),
            ("regions", endpoint.topology_hints.regions.as_slice()),
            (
                "availabilityZones",
                endpoint.topology_hints.availability_zones.as_slice(),
            ),
        ] {
            for (hint_index, hint) in hints.iter().enumerate() {
                validate_token(
                    &format!("endpoints[{index}].topologyHints.{field}[{hint_index}]"),
                    hint,
                )?;
            }
        }
    }
    for (label, value) in &spec.node_selector {
        validate_nonempty("nodeSelector label", label)?;
        validate_nonempty(&format!("nodeSelector[{label}]"), value)?;
    }
    let mut catalog_names = BTreeMap::<String, usize>::new();
    for (index, reference) in spec.catalog_refs.iter().enumerate() {
        validate_token(&format!("catalogRefs[{index}].name"), &reference.name)?;
        if let Some(previous) = catalog_names.insert(reference.name.clone(), index) {
            return Err(format!(
                "catalogRefs[{index}] duplicates catalogRefs[{previous}]"
            ));
        }
    }
    let mut key_refs = BTreeMap::<(String, String, String), usize>::new();
    for (index, reference) in spec.trusted_public_key_refs.iter().enumerate() {
        validate_token(
            &format!("trustedPublicKeyRefs[{index}].namespace"),
            &reference.namespace,
        )?;
        validate_token(
            &format!("trustedPublicKeyRefs[{index}].name"),
            &reference.name,
        )?;
        validate_token(
            &format!("trustedPublicKeyRefs[{index}].key"),
            &reference.key,
        )?;
        let key = (
            reference.namespace.clone(),
            reference.name.clone(),
            reference.key.clone(),
        );
        if let Some(previous) = key_refs.insert(key, index) {
            return Err(format!(
                "trustedPublicKeyRefs[{index}] duplicates trustedPublicKeyRefs[{previous}]"
            ));
        }
    }
    Ok(())
}

fn validate_source_url(index: usize, endpoint: &KernelModuleSourceEndpoint) -> Result<(), String> {
    let url = endpoint.base_url.trim_end_matches('/');
    if url != endpoint.base_url {
        return Err(format!(
            "endpoints[{index}].baseUrl must not have a trailing slash"
        ));
    }
    let remainder = if let Some(remainder) = url.strip_prefix("https://") {
        remainder
    } else if let Some(remainder) = url.strip_prefix("http://") {
        if !endpoint.allow_insecure_http {
            return Err(format!(
                "endpoints[{index}] uses plain HTTP without allowInsecureHttp=true"
            ));
        }
        remainder
    } else {
        return Err(format!(
            "endpoints[{index}].baseUrl must use https:// or explicitly permitted http://"
        ));
    };
    let authority = remainder.split('/').next().unwrap_or("");
    if authority.is_empty()
        || authority.contains('@')
        || endpoint.base_url.contains('?')
        || endpoint.base_url.contains('#')
    {
        return Err(format!(
            "endpoints[{index}].baseUrl must have a host and must not contain credentials, query, or fragment"
        ));
    }
    Ok(())
}

pub fn validate_bundle_spec(spec: &ZccusanKernelModuleBundleSpec) -> Result<(), String> {
    validate_artifact("manifest", &spec.manifest)?;
    validate_artifact("module.object", &spec.module.object)?;
    validate_token("module.name", &spec.module.name)?;
    validate_compatibility(&spec.compatibility)?;
    validate_build(&spec.build)?;
    if spec.signatures.is_empty() {
        return Err("signatures must contain at least one detached manifest signature".into());
    }
    for (index, signature) in spec.signatures.iter().enumerate() {
        validate_token(&format!("signatures[{index}].format"), &signature.format)?;
        validate_artifact(&format!("signatures[{index}].object"), &signature.object)?;
    }
    for (index, attestation) in spec.attestations.iter().enumerate() {
        validate_token(&format!("attestations[{index}].name"), &attestation.name)?;
        validate_artifact(
            &format!("attestations[{index}].object"),
            &attestation.object,
        )?;
    }
    Ok(())
}

pub fn validate_catalog_spec(spec: &ZccusanKernelModuleCatalogSpec) -> Result<(), String> {
    if spec.catalog_generation == 0 {
        return Err("catalogGeneration must be greater than zero".into());
    }
    validate_artifact("catalog", &spec.catalog)?;
    if spec.entries.is_empty() {
        return Err("entries must not be empty".into());
    }
    if spec.signatures.is_empty() {
        return Err("signatures must contain at least one detached catalog signature".into());
    }

    let mut unique = BTreeMap::<(String, String, String, Option<String>), String>::new();
    for (index, entry) in spec.entries.iter().enumerate() {
        validate_token(&format!("entries[{index}].moduleName"), &entry.module_name)?;
        validate_architecture(&entry.architecture)?;
        validate_nonempty(
            &format!("entries[{index}].kernelRelease"),
            &entry.kernel_release,
        )?;
        validate_interfaces(&entry.interfaces)?;
        validate_artifact(&format!("entries[{index}].manifest"), &entry.manifest)?;
        let key = (
            entry.module_name.clone(),
            entry.architecture.clone(),
            entry.kernel_release.clone(),
            entry.kernel_build_id.clone(),
        );
        if let Some(previous) = unique.insert(key, entry.manifest.sha256.clone()) {
            return Err(format!(
                "entries[{index}] duplicates a compatibility match already mapped to {previous}"
            ));
        }
    }
    for (index, signature) in spec.signatures.iter().enumerate() {
        validate_token(&format!("signatures[{index}].format"), &signature.format)?;
        validate_artifact(&format!("signatures[{index}].object"), &signature.object)?;
    }
    Ok(())
}

pub fn validate_module_against_bundle(
    observed: &ObservedKernelModule,
    spec: &ZccusanKernelModuleBundleSpec,
) -> Result<(), String> {
    validate_bundle_spec(spec)?;
    let expected = &spec.compatibility;
    let checks = [
        (
            "module name",
            observed.module_name.as_str(),
            spec.module.name.as_str(),
        ),
        (
            "architecture",
            observed.architecture.as_str(),
            expected.architecture.as_str(),
        ),
        (
            "kernel release",
            observed.kernel_release.as_str(),
            expected.kernel_release.as_str(),
        ),
        (
            "vermagic",
            observed.vermagic.as_str(),
            expected.vermagic.as_str(),
        ),
        (
            "sha256",
            observed.sha256.as_str(),
            spec.module.object.sha256.as_str(),
        ),
    ];
    for (field, actual, declared) in checks {
        if actual != declared {
            return Err(format!(
                "{field} mismatch: module observed {actual:?}, bundle declared {declared:?}"
            ));
        }
    }
    if observed.size_bytes != spec.module.object.size_bytes {
        return Err(format!(
            "size mismatch: module observed {}, bundle declared {}",
            observed.size_bytes, spec.module.object.size_bytes
        ));
    }
    Ok(())
}

pub fn inspect_kernel_module(path: impl AsRef<Path>) -> Result<ObservedKernelModule, String> {
    let bytes = fs::read(path.as_ref())
        .map_err(|error| format!("read {}: {error}", path.as_ref().display()))?;
    inspect_kernel_module_bytes(&bytes)
}

fn validate_artifact(field: &str, artifact: &ContentAddressedArtifact) -> Result<(), String> {
    validate_nonempty(&format!("{field}.mediaType"), &artifact.media_type)?;
    validate_sha256(&format!("{field}.sha256"), &artifact.sha256)?;
    if artifact.size_bytes == 0 {
        return Err(format!("{field}.sizeBytes must be greater than zero"));
    }
    let path = Path::new(&artifact.object_path);
    if path.is_absolute()
        || artifact.object_path.is_empty()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(format!(
            "{field}.objectPath must be a non-empty relative immutable key without traversal"
        ));
    }
    if path.file_name().and_then(|part| part.to_str()) != Some(artifact.sha256.as_str()) {
        return Err(format!(
            "{field}.objectPath final component must equal its sha256 digest"
        ));
    }
    Ok(())
}

fn validate_compatibility(value: &KernelModuleCompatibility) -> Result<(), String> {
    validate_architecture(&value.architecture)?;
    validate_nonempty("compatibility.kernelRelease", &value.kernel_release)?;
    validate_nonempty("compatibility.vermagic", &value.vermagic)?;
    let vermagic_release = value.vermagic.split_ascii_whitespace().next().unwrap_or("");
    if vermagic_release != value.kernel_release {
        return Err(format!(
            "compatibility.kernelRelease {:?} is not the first vermagic field {:?}",
            value.kernel_release, vermagic_release
        ));
    }
    validate_interfaces(&value.interfaces)
}

fn validate_interfaces(value: &KernelModuleInterfaces) -> Result<(), String> {
    if value.minimum_userspace_protocol > value.maximum_userspace_protocol {
        return Err(
            "interfaces.minimumUserspaceProtocol must not exceed maximumUserspaceProtocol".into(),
        );
    }
    Ok(())
}

fn validate_build(value: &KernelModuleBuildProvenance) -> Result<(), String> {
    validate_nonempty("build.sourceRevision", &value.source_revision)?;
    validate_sha256("build.sourceTreeSha256", &value.source_tree_sha256)?;
    validate_sha256("build.buildRecipeSha256", &value.build_recipe_sha256)?;
    validate_sha256("build.kernelHeadersSha256", &value.kernel_headers_sha256)?;
    validate_sha256("build.kernelConfigSha256", &value.kernel_config_sha256)?;
    validate_sha256("build.moduleSymversSha256", &value.module_symvers_sha256)?;
    validate_nonempty("build.builtAt", &value.built_at)?;
    let Some(digest) = value.toolchain_image_digest.strip_prefix("sha256:") else {
        return Err("build.toolchainImageDigest must be an immutable sha256 OCI digest".into());
    };
    validate_sha256("build.toolchainImageDigest", digest)
}

fn validate_architecture(value: &str) -> Result<(), String> {
    validate_token("architecture", value)?;
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err("architecture must use lowercase OCI spelling".into());
    }
    Ok(())
}

fn validate_token(field: &str, value: &str) -> Result<(), String> {
    validate_nonempty(field, value)?;
    if value
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    {
        return Err(format!(
            "{field} may contain only ASCII letters, digits, dot, underscore, or hyphen"
        ));
    }
    Ok(())
}

fn validate_nonempty(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.trim() != value {
        Err(format!(
            "{field} must be non-empty and have no surrounding whitespace"
        ))
    } else {
        Ok(())
    }
}

fn validate_sha256(field: &str, value: &str) -> Result<(), String> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    {
        Err(format!(
            "{field} must be exactly 64 lowercase hexadecimal characters"
        ))
    } else {
        Ok(())
    }
}

fn inspect_kernel_module_bytes(bytes: &[u8]) -> Result<ObservedKernelModule, String> {
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" {
        return Err("kernel module is not an ELF object".into());
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err("only 64-bit little-endian kernel-module ELF objects are supported".into());
    }
    let architecture = match read_u16(bytes, 18)? {
        62 => "amd64",
        183 => "arm64",
        243 => "riscv64",
        21 => "ppc64le",
        machine => return Err(format!("unsupported ELF machine {machine}")),
    }
    .to_string();
    let section_offset = usize_from_u64(read_u64(bytes, 40)?, "section table offset")?;
    let section_entry_size = usize::from(read_u16(bytes, 58)?);
    let section_count = usize::from(read_u16(bytes, 60)?);
    let section_names_index = usize::from(read_u16(bytes, 62)?);
    if section_entry_size < 64 || section_count == 0 || section_names_index >= section_count {
        return Err("invalid ELF section table".into());
    }
    let section_names = section_data(
        bytes,
        section_offset,
        section_entry_size,
        section_count,
        section_names_index,
    )?;
    let mut modinfo = None;
    for index in 0..section_count {
        let header = checked_slice(
            bytes,
            section_offset
                .checked_add(
                    index
                        .checked_mul(section_entry_size)
                        .ok_or("section overflow")?,
                )
                .ok_or("section overflow")?,
            section_entry_size,
            "section header",
        )?;
        let name_offset = usize::try_from(read_u32(header, 0)?)
            .map_err(|_| "section name offset is too large")?;
        let name = nul_string(section_names, name_offset)?;
        if name == ".modinfo" {
            modinfo = Some(section_data(
                bytes,
                section_offset,
                section_entry_size,
                section_count,
                index,
            )?);
            break;
        }
    }
    let modinfo = modinfo.ok_or("ELF object has no .modinfo section")?;
    let fields = modinfo
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .filter_map(|value| std::str::from_utf8(value).ok())
        .filter_map(|value| value.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let module_name = fields
        .get("name")
        .ok_or(".modinfo has no name field")?
        .to_string();
    let vermagic = fields
        .get("vermagic")
        .ok_or(".modinfo has no vermagic field")?
        .to_string();
    let kernel_release = vermagic
        .split_ascii_whitespace()
        .next()
        .ok_or("vermagic is empty")?
        .to_string();
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    Ok(ObservedKernelModule {
        module_name,
        architecture,
        kernel_release,
        vermagic,
        sha256,
        size_bytes: u64::try_from(bytes.len()).map_err(|_| "module is too large")?,
        embedded_signature_present: bytes.ends_with(b"~Module signature appended~\n"),
    })
}

fn section_data(
    bytes: &[u8],
    table_offset: usize,
    entry_size: usize,
    count: usize,
    index: usize,
) -> Result<&[u8], String> {
    if index >= count {
        return Err("section index is out of bounds".into());
    }
    let header_offset = table_offset
        .checked_add(
            index
                .checked_mul(entry_size)
                .ok_or("section header overflow")?,
        )
        .ok_or("section header overflow")?;
    let header = checked_slice(bytes, header_offset, entry_size, "section header")?;
    let offset = usize_from_u64(read_u64(header, 24)?, "section offset")?;
    let size = usize_from_u64(read_u64(header, 32)?, "section size")?;
    checked_slice(bytes, offset, size, "section data")
}

fn checked_slice<'a>(
    bytes: &'a [u8],
    offset: usize,
    size: usize,
    field: &str,
) -> Result<&'a [u8], String> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| format!("{field} overflows"))?;
    bytes
        .get(offset..end)
        .ok_or_else(|| format!("{field} is out of bounds"))
}

fn nul_string(bytes: &[u8], offset: usize) -> Result<&str, String> {
    let value = bytes.get(offset..).ok_or("section name is out of bounds")?;
    let end = value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    std::str::from_utf8(&value[..end]).map_err(|_| "section name is not UTF-8".into())
}

fn usize_from_u64(value: u64, field: &str) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("{field} is too large"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let value = checked_slice(bytes, offset, 2, "u16 field")?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = checked_slice(bytes, offset, 4, "u32 field")?;
    Ok(u32::from_le_bytes(
        value.try_into().expect("four-byte slice"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let value = checked_slice(bytes, offset, 8, "u64 field")?;
    Ok(u64::from_le_bytes(
        value.try_into().expect("eight-byte slice"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;
    use serde::Deserialize;

    fn digest(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn object(byte: char, size_bytes: u64) -> ContentAddressedArtifact {
        let sha256 = digest(byte);
        ContentAddressedArtifact {
            media_type: "application/octet-stream".into(),
            object_path: format!("objects/sha256/{sha256}"),
            sha256,
            size_bytes,
        }
    }

    fn interfaces() -> KernelModuleInterfaces {
        KernelModuleInterfaces {
            zcnblk_client_edge_abi: 1,
            zcnblk_shared_memory_abi: 1,
            minimum_userspace_protocol: 1,
            maximum_userspace_protocol: 2,
        }
    }

    fn bundle_spec(module_sha: String, module_size: u64) -> ZccusanKernelModuleBundleSpec {
        let mut module_object = object('b', module_size);
        module_object.sha256 = module_sha.clone();
        module_object.object_path = format!("objects/sha256/{module_sha}");
        ZccusanKernelModuleBundleSpec {
            manifest: object('a', 512),
            module: KernelModuleArtifact {
                name: "zcnblk_client_mod".into(),
                object: module_object,
            },
            compatibility: KernelModuleCompatibility {
                architecture: "amd64".into(),
                kernel_release: "7.2.0".into(),
                vermagic: "7.2.0 SMP preempt mod_unload".into(),
                kernel_build_id: None,
                interfaces: interfaces(),
            },
            build: KernelModuleBuildProvenance {
                source_revision: "deadbeef".into(),
                source_tree_sha256: digest('c'),
                build_recipe_sha256: digest('d'),
                toolchain_image_digest: format!("sha256:{}", digest('e')),
                kernel_headers_sha256: digest('f'),
                kernel_config_sha256: digest('1'),
                module_symvers_sha256: digest('2'),
                built_at: "2026-08-26T12:00:00Z".into(),
            },
            signatures: vec![ArtifactSignature {
                format: "Dsse".into(),
                object: object('3', 128),
            }],
            attestations: vec![],
        }
    }

    fn synthetic_module() -> Vec<u8> {
        let modinfo = b"name=zcnblk_client_mod\0vermagic=7.2.0 SMP preempt mod_unload\0";
        let names = b"\0.modinfo\0.shstrtab\0";
        let modinfo_offset = 64usize;
        let names_offset = 128usize;
        let section_offset = 192usize;
        let mut bytes = vec![0u8; section_offset + 3 * 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[40..48].copy_from_slice(&(section_offset as u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&3u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&2u16.to_le_bytes());
        bytes[modinfo_offset..modinfo_offset + modinfo.len()].copy_from_slice(modinfo);
        bytes[names_offset..names_offset + names.len()].copy_from_slice(names);

        let modinfo_header = section_offset + 64;
        bytes[modinfo_header..modinfo_header + 4].copy_from_slice(&1u32.to_le_bytes());
        bytes[modinfo_header + 24..modinfo_header + 32]
            .copy_from_slice(&(modinfo_offset as u64).to_le_bytes());
        bytes[modinfo_header + 32..modinfo_header + 40]
            .copy_from_slice(&(modinfo.len() as u64).to_le_bytes());
        let names_header = section_offset + 128;
        bytes[names_header..names_header + 4].copy_from_slice(&10u32.to_le_bytes());
        bytes[names_header + 24..names_header + 32]
            .copy_from_slice(&(names_offset as u64).to_le_bytes());
        bytes[names_header + 32..names_header + 40]
            .copy_from_slice(&(names.len() as u64).to_le_bytes());
        bytes
    }

    #[test]
    fn prefixed_crds_are_structural_and_cluster_scoped() {
        for crd in [
            ZccusanKernelModuleSource::crd(),
            ZccusanKernelModuleBundle::crd(),
            ZccusanKernelModuleCatalog::crd(),
        ] {
            assert_eq!(crd.spec.scope, "Cluster");
            assert!(crd.spec.versions[0].schema.is_some());
            assert!(
                crd.metadata
                    .name
                    .unwrap()
                    .starts_with("zccusankernelmodule")
            );
        }
    }

    #[test]
    fn inspector_observes_elf_instead_of_accepting_manifest_claims() {
        let bytes = synthetic_module();
        let observed = inspect_kernel_module_bytes(&bytes).unwrap();
        assert_eq!(observed.module_name, "zcnblk_client_mod");
        assert_eq!(observed.architecture, "amd64");
        assert_eq!(observed.kernel_release, "7.2.0");
        let spec = bundle_spec(observed.sha256.clone(), observed.size_bytes);
        validate_module_against_bundle(&observed, &spec).unwrap();

        let mut false_claim = spec;
        false_claim.compatibility.architecture = "arm64".into();
        assert!(validate_module_against_bundle(&observed, &false_claim).is_err());
    }

    #[test]
    fn content_addressed_paths_and_catalog_matches_fail_closed() {
        let observed = inspect_kernel_module_bytes(&synthetic_module()).unwrap();
        let mut spec = bundle_spec(observed.sha256, observed.size_bytes);
        spec.module.object.object_path = "latest/zcnblk_client_mod.ko".into();
        assert!(validate_bundle_spec(&spec).is_err());

        let entry = KernelModuleCatalogEntry {
            module_name: "zcnblk_client_mod".into(),
            architecture: "amd64".into(),
            kernel_release: "7.2.0".into(),
            kernel_build_id: None,
            interfaces: interfaces(),
            manifest: object('a', 512),
        };
        let catalog = ZccusanKernelModuleCatalogSpec {
            catalog_generation: 1,
            catalog: object('4', 1024),
            entries: vec![entry.clone(), entry],
            signatures: vec![ArtifactSignature {
                format: "Dsse".into(),
                object: object('5', 128),
            }],
        };
        assert!(validate_catalog_spec(&catalog).is_err());
    }

    #[test]
    fn malformed_elf_bounds_checks_return_errors() {
        let mut bytes = synthetic_module();
        bytes[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(inspect_kernel_module_bytes(&bytes).is_err());
    }

    #[test]
    fn source_policy_has_no_default_origin_and_scopes_trust_by_selector() {
        let source = ZccusanKernelModuleSourceSpec {
            enabled: true,
            node_selector: BTreeMap::from([(
                "topology.kubernetes.io/region".into(),
                "us-east-1".into(),
            )]),
            endpoints: vec![KernelModuleSourceEndpoint {
                base_url: "https://artifacts.us-east-1.example.test/zccusan".into(),
                allow_insecure_http: false,
                priority: 10,
                topology_hints: KernelModuleSourceTopologyHints {
                    regions: vec!["us-east-1".into()],
                    ..KernelModuleSourceTopologyHints::default()
                },
            }],
            catalog_refs: vec![KernelModuleCatalogReference {
                name: "production".into(),
            }],
            trusted_public_key_refs: vec![PublicKeyConfigMapReference {
                namespace: "zcblock-system".into(),
                name: "kernel-module-roots".into(),
                key: "release.pem".into(),
            }],
        };
        validate_source_spec(&source).unwrap();
        let mut insecure = source;
        insecure.endpoints[0].base_url = "http://artifacts.example.test".into();
        assert!(validate_source_spec(&insecure).is_err());
    }

    #[test]
    fn published_example_deserializes_and_passes_semantic_validation() {
        let source =
            include_str!("../zccusan/deploy/zcblock-csi/kernel-module-artifacts.example.yaml");
        let mut documents = serde_yaml::Deserializer::from_str(source);
        let source: ZccusanKernelModuleSource =
            ZccusanKernelModuleSource::deserialize(documents.next().unwrap()).unwrap();
        let catalog: ZccusanKernelModuleCatalog =
            ZccusanKernelModuleCatalog::deserialize(documents.next().unwrap()).unwrap();
        let bundle: ZccusanKernelModuleBundle =
            ZccusanKernelModuleBundle::deserialize(documents.next().unwrap()).unwrap();
        assert!(documents.next().is_none());
        validate_source_spec(&source.spec).unwrap();
        validate_catalog_spec(&catalog.spec).unwrap();
        validate_bundle_spec(&bundle.spec).unwrap();
    }
}
