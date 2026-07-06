use anyhow::{anyhow, Context, Result};
use reqwest::header::AUTHORIZATION;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

pub const API_BASE_URL: &str = "https://cloud.lambdalabs.com/api/v1";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Error, Debug)]
pub enum LambdaError {
    #[error("API key not set. Set LAMBDA_API_KEY or LAMBDA_API_KEY_COMMAND environment variable")]
    ApiKeyNotSet,
    #[error("Failed to execute API key command: {0}")]
    ApiKeyCommandFailed(String),
    #[error("Instance type '{0}' not found")]
    InstanceTypeNotFound(String),
    #[error("No regions available for instance type '{0}'")]
    NoRegionsAvailable(String),
    #[error("No instance IDs returned from launch request")]
    NoInstanceIds,
    #[error("API request failed: {0}")]
    ApiError(String),
    #[error("SSH key is required for this operation")]
    SshKeyRequired,
}

#[derive(Deserialize, Debug)]
pub struct ApiResponse<T> {
    pub data: T,
}

#[derive(Deserialize, Debug)]
pub struct ApiErrorResponse {
    pub error: ApiErrorDetail,
}

#[derive(Deserialize, Debug)]
pub struct ApiErrorDetail {
    pub message: String,
}

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct Instance {
    pub id: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub ip: Option<String>,
    pub ssh_key_names: Option<Vec<String>>,
    pub instance_type: Option<InstanceTypeInfo>,
    pub region: Option<RegionInfo>,
}

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct InstanceTypeInfo {
    pub name: Option<String>,
}

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct RegionInfo {
    pub name: Option<String>,
}

/// Filesystem (persistent storage) information
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct Filesystem {
    pub id: String,
    pub name: String,
    pub mount_point: String,
    pub created: String,
    pub region: FilesystemRegion,
    pub is_in_use: bool,
    #[serde(default)]
    pub bytes_used: u64,
}

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct FilesystemRegion {
    pub name: String,
    pub description: String,
}

#[derive(Deserialize, Debug)]
pub struct LaunchResponse {
    pub instance_ids: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct InstanceTypeData {
    pub name: String,
    pub description: String,
    pub price_cents_per_hour: i32,
    pub vcpus: u32,
    pub memory_gib: u32,
    pub storage_gib: u32,
    pub regions_available: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InstanceTypeResponse {
    pub instance_type: InstanceType,
    pub regions_with_capacity_available: Vec<Region>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InstanceType {
    pub description: String,
    pub price_cents_per_hour: i32,
    pub specs: InstanceSpecs,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InstanceSpecs {
    pub vcpus: u32,
    pub memory_gib: u32,
    pub storage_gib: u32,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Region {
    pub name: String,
    #[allow(dead_code)]
    pub description: String,
}

/// Source for the API key - either a direct value or a command to execute
#[derive(Debug, Clone)]
enum ApiKeySource {
    /// Direct API key value (already resolved)
    Direct(String),
    /// Command to execute to get the API key (lazy evaluation)
    Command(String),
}

/// Reference to a base image for launch. The Lambda API `image` object accepts
/// exactly one of `id` (a specific image) or `family` (newest in the family).
#[derive(Debug, Clone, Copy)]
pub enum ImageRef<'a> {
    Id(&'a str),
    Family(&'a str),
}

impl<'a> ImageRef<'a> {
    /// Classify a raw value as an image id or a family and build the matching
    /// `ImageRef`. Image ids are UUIDs and families are human-readable slugs
    /// (e.g. `lambda-stack-24-04`), so the two never collide — this backs the
    /// forgiving `--image` CLI flag and the MCP `image` param.
    pub fn smart(value: &'a str) -> Self {
        if looks_like_image_id(value) {
            ImageRef::Id(value)
        } else {
            ImageRef::Family(value)
        }
    }
}

/// A Lambda image id is a UUID (32 hex digits, optionally hyphenated); families
/// are human slugs that always contain non-hex letters, so "exactly 32 hex digits
/// once hyphens are removed" cleanly tells an id from a family.
fn looks_like_image_id(value: &str) -> bool {
    let mut hex_digits = 0usize;
    for c in value.chars() {
        match c {
            '-' => {}
            c if c.is_ascii_hexdigit() => hex_digits += 1,
            _ => return false,
        }
    }
    hex_digits == 32
}

/// Canonical form of an image id for comparison: hyphens removed, lowercased. Lets
/// a user pass a UUID in any hyphenation/case and still match the `/images` form.
fn canonical_id(id: &str) -> String {
    id.chars()
        .filter(|c| *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Numeric components of a version, for ordering (so `24.4.10` sorts after `24.4.9`).
pub fn version_key(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

/// A base image returned by `GET /images`. Every field except `id` is optional so
/// a null or omitted value in the response doesn't fail the whole list. `region`
/// reuses `RegionInfo` (name-only, lenient), ignoring the region's `description`.
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct Image {
    pub id: String,
    pub name: Option<String>,
    pub family: Option<String>,
    pub version: Option<String>,
    pub architecture: Option<String>,
    pub description: Option<String>,
    pub region: Option<RegionInfo>,
}

/// `/images` collapsed to one entry per (family, architecture): the newest version
/// in that group and every region it is offered in. Shared by the `lambda images`
/// default view and the MCP `list_images` tool so their granularity stays in sync.
#[derive(Debug, Clone)]
pub struct ImageBuild {
    pub family: String,
    pub arch: String,
    pub latest_version: String,
    pub regions: Vec<String>,
}

impl ImageBuild {
    /// How many regions to list inline before collapsing to a count.
    const REGION_LIST_MAX: usize = 4;

    /// Human-readable region summary shared by the CLI and MCP grouped views: the
    /// full list when there are a few, a count otherwise, `-` when none.
    pub fn regions_summary(&self) -> String {
        match self.regions.len() {
            0 => "-".to_string(),
            n if n <= Self::REGION_LIST_MAX => self.regions.join(", "),
            n => format!("{n} regions"),
        }
    }
}

/// Group raw `/images` objects (one per id and region) into one `ImageBuild` per
/// (family, arch), keeping the newest version and the sorted, de-duplicated region
/// set. Consumes `images` to avoid copies. Ordered by (family, arch).
///
/// Images with no family are excluded: they can't be launched via
/// `--image-family` / a family `image` param, so surfacing them here would present
/// an unlaunchable value. `lambda images --all` still lists them (launchable by id).
pub fn group_image_builds(images: Vec<Image>) -> Vec<ImageBuild> {
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct Acc {
        latest_version: String,
        regions: BTreeSet<String>,
    }

    let mut builds: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for img in images {
        // Family-less images aren't launchable by family; leave them to `--all`.
        let Some(family) = img.family.filter(|f| !f.is_empty()) else {
            continue;
        };
        let arch = img.architecture.unwrap_or_else(|| "-".to_string());
        // Treat a null OR empty version as the "-" placeholder so the is_empty()
        // sentinel below only ever fires on the accumulator's initial state.
        let version = img
            .version
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "-".to_string());
        let acc = builds.entry((family, arch)).or_default();
        if acc.latest_version.is_empty() || version_key(&version) > version_key(&acc.latest_version)
        {
            acc.latest_version = version;
        }
        if let Some(name) = img.region.and_then(|r| r.name) {
            acc.regions.insert(name);
        }
    }

    builds
        .into_iter()
        .map(|((family, arch), acc)| ImageBuild {
            family,
            arch,
            latest_version: acc.latest_version,
            regions: acc.regions.into_iter().collect(),
        })
        .collect()
}

/// Lambda API client
pub struct LambdaClient {
    client: Client,
    api_key_source: ApiKeySource,
    /// Cached API key (used for lazy evaluation)
    cached_api_key: Mutex<Option<String>>,
}

impl LambdaClient {
    pub fn new(api_key: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            client,
            api_key_source: ApiKeySource::Direct(api_key),
            cached_api_key: Mutex::new(None),
        })
    }

    /// Create a client with a lazy API key source (command executed on first use)
    fn new_lazy(api_key_source: ApiKeySource) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            client,
            api_key_source,
            cached_api_key: Mutex::new(None),
        })
    }

    /// Create a client using environment variables for the API key.
    ///
    /// Checks in order:
    /// 1. `LAMBDA_API_KEY` - Direct API key
    /// 2. `LAMBDA_API_KEY_COMMAND` - Command to execute to get the API key (e.g., `op read op://vault/lambda/api-key`)
    ///
    /// By default, if `LAMBDA_API_KEY_COMMAND` is used, the command is executed immediately.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with_options(false)
    }

    /// Create a client using environment variables for the API key with options.
    ///
    /// If `lazy` is true and `LAMBDA_API_KEY_COMMAND` is used, the command execution
    /// is deferred until the first API request.
    pub fn from_env_with_options(lazy: bool) -> Result<Self> {
        // First, try direct API key (always immediate)
        if let Ok(key) = std::env::var("LAMBDA_API_KEY") {
            if !key.is_empty() {
                return Self::new(key);
            }
        }

        // Then, try command-based retrieval
        if let Ok(command) = std::env::var("LAMBDA_API_KEY_COMMAND") {
            if !command.is_empty() {
                if lazy {
                    // Defer command execution until first API request
                    return Self::new_lazy(ApiKeySource::Command(command));
                } else {
                    // Execute command immediately (default behavior)
                    let key = execute_api_key_command(&command)?;
                    return Self::new(key);
                }
            }
        }

        Err(LambdaError::ApiKeyNotSet.into())
    }

    /// Get the API key, executing the command if necessary (lazy evaluation)
    fn get_api_key(&self) -> Result<String> {
        match &self.api_key_source {
            ApiKeySource::Direct(key) => Ok(key.clone()),
            ApiKeySource::Command(cmd) => {
                let mut cache = self
                    .cached_api_key
                    .lock()
                    .map_err(|e| anyhow!("Failed to acquire lock: {}", e))?;

                if let Some(key) = cache.as_ref() {
                    return Ok(key.clone());
                }

                let key = execute_api_key_command(cmd)?;
                *cache = Some(key.clone());
                Ok(key)
            }
        }
    }

    /// Validate the API key by making a test request
    pub async fn validate_api_key(&self) -> Result<()> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/instances", API_BASE_URL);
        let response = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to connect to Lambda API")?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let error_msg = Self::parse_error_response(response).await;
            Err(anyhow!(
                "API key validation failed ({}): {}",
                status,
                error_msg
            ))
        }
    }

    /// List all available instance types
    pub async fn list_instance_types(&self) -> Result<Vec<InstanceTypeData>> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/instance-types", API_BASE_URL);
        let response = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to fetch instance types")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to list instances: {}", error_msg));
        }

        let response: ApiResponse<HashMap<String, InstanceTypeResponse>> = response
            .json()
            .await
            .context("Failed to parse instance types response")?;

        let mut result: Vec<InstanceTypeData> = response
            .data
            .into_iter()
            .map(|(name, data)| InstanceTypeData {
                name,
                description: data.instance_type.description,
                price_cents_per_hour: data.instance_type.price_cents_per_hour,
                vcpus: data.instance_type.specs.vcpus,
                memory_gib: data.instance_type.specs.memory_gib,
                storage_gib: data.instance_type.specs.storage_gib,
                regions_available: data
                    .regions_with_capacity_available
                    .into_iter()
                    .map(|r| r.name)
                    .collect(),
            })
            .collect();

        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    /// Get instance type details (for checking availability)
    pub async fn get_instance_type(&self, gpu: &str) -> Result<Option<InstanceTypeResponse>> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/instance-types", API_BASE_URL);
        let response = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to fetch instance types")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to get instance types: {}", error_msg));
        }

        let response: ApiResponse<HashMap<String, InstanceTypeResponse>> = response
            .json()
            .await
            .context("Failed to parse instance types")?;

        Ok(response.data.get(gpu).cloned())
    }

    /// Launch a new instance
    pub async fn launch_instance(
        &self,
        gpu: &str,
        ssh_key: &str,
        name: Option<&str>,
        region: Option<&str>,
    ) -> Result<LaunchResult> {
        self.launch_instance_with_filesystem(gpu, ssh_key, name, region, None, None)
            .await
    }

    /// Launch a new instance with optional filesystem attachment
    pub async fn launch_instance_with_filesystem(
        &self,
        gpu: &str,
        ssh_key: &str,
        name: Option<&str>,
        region: Option<&str>,
        image: Option<ImageRef<'_>>,
        filesystem: Option<&str>,
    ) -> Result<LaunchResult> {
        let instance_type_response = self
            .get_instance_type(gpu)
            .await?
            .ok_or_else(|| LambdaError::InstanceTypeNotFound(gpu.to_string()))?;

        let capacity: Vec<String> = instance_type_response
            .regions_with_capacity_available
            .iter()
            .map(|r| r.name.clone())
            .collect();

        // Resolve the requested image once (single GET /images). A pinned id is
        // validated + canonicalized (users may pass any hyphenation/case) and
        // constrains region selection to where it exists; a family is validated but
        // region-agnostic. This lives here so every caller (CLI and MCP) behaves
        // the same.
        let (image_value, image_regions): (Option<serde_json::Value>, Option<Vec<String>>) =
            match image {
                Some(ImageRef::Id(id)) => {
                    let (canonical, regions) = self.resolve_image_id(id).await?;
                    let regions = (!regions.is_empty()).then_some(regions);
                    (Some(serde_json::json!({ "id": canonical })), regions)
                }
                Some(ImageRef::Family(family)) => {
                    self.validate_image_family(family).await?;
                    (Some(serde_json::json!({ "family": family })), None)
                }
                None => (None, None),
            };

        let region_name = match region {
            Some(r) => {
                if !capacity.iter().any(|c| c.as_str() == r) {
                    return Err(anyhow!(
                        "Region '{}' is not available for instance type '{}'. Available regions: {}",
                        r,
                        gpu,
                        capacity.join(", ")
                    ));
                }
                if let Some(ref regions) = image_regions {
                    if !regions.iter().any(|ir| ir.as_str() == r) {
                        return Err(anyhow!(
                            "Region '{}' does not offer the requested image (available in: {})",
                            r,
                            regions.join(", ")
                        ));
                    }
                }
                r.to_string()
            }
            None => match &image_regions {
                Some(regions) => capacity
                    .iter()
                    .find(|c| regions.contains(*c))
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!(
                            "The requested image is available in [{}], but none of those regions currently have {} capacity.",
                            regions.join(", "),
                            gpu
                        )
                    })?,
                None => capacity
                    .first()
                    .cloned()
                    .ok_or_else(|| LambdaError::NoRegionsAvailable(gpu.to_string()))?,
            },
        };

        let url = format!("{}/instance-operations/launch", API_BASE_URL);

        let mut payload = serde_json::json!({
            "region_name": region_name,
            "instance_type_name": gpu,
            "ssh_key_names": [ssh_key],
            "quantity": 1
        });

        if let Some(instance_name) = name {
            payload["name"] = serde_json::Value::String(instance_name.to_string());
        }

        if let Some(image_value) = image_value {
            payload["image"] = image_value;
        }

        if let Some(fs_name) = filesystem {
            payload["file_system_names"] = serde_json::json!([fs_name]);
        }

        let api_key = self.get_api_key()?;
        let response = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .json(&payload)
            .send()
            .await
            .context("Failed to send launch request")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to launch instance: {}", error_msg));
        }

        let parsed_response: ApiResponse<LaunchResponse> = response
            .json()
            .await
            .context("Failed to parse launch response")?;

        let instance_id = parsed_response
            .data
            .instance_ids
            .first()
            .ok_or(LambdaError::NoInstanceIds)?
            .clone();

        Ok(LaunchResult {
            instance_id,
            region: region_name,
        })
    }

    /// Terminate an instance
    pub async fn terminate_instance(&self, instance_id: &str) -> Result<()> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/instance-operations/terminate", API_BASE_URL);
        let payload = serde_json::json!({
            "instance_ids": [instance_id]
        });

        let response = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .json(&payload)
            .send()
            .await
            .context("Failed to send terminate request")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to terminate instance: {}", error_msg));
        }

        Ok(())
    }

    /// List all running instances
    pub async fn list_running_instances(&self) -> Result<Vec<Instance>> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/instances", API_BASE_URL);
        let response = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to fetch running instances")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to list running instances: {}", error_msg));
        }

        let response: ApiResponse<Vec<Instance>> = response
            .json()
            .await
            .context("Failed to parse running instances response")?;

        Ok(response.data)
    }

    /// Get details for a specific instance
    pub async fn get_instance(&self, instance_id: &str) -> Result<Instance> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/instances/{}", API_BASE_URL, instance_id);
        let response = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to fetch instance details")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!(
                "Failed to get instance details ({}): {}",
                status,
                error_msg
            ));
        }

        let response: ApiResponse<Instance> = response
            .json()
            .await
            .context("Failed to parse instance details")?;

        Ok(response.data)
    }

    /// Check if a GPU type is available
    pub async fn check_availability(&self, gpu: &str) -> Result<Vec<String>> {
        let instance_type = self
            .get_instance_type(gpu)
            .await?
            .ok_or_else(|| LambdaError::InstanceTypeNotFound(gpu.to_string()))?;

        Ok(instance_type
            .regions_with_capacity_available
            .into_iter()
            .map(|r| r.name)
            .collect())
    }

    /// Resolve a pinned image id (accepted in any hyphenation/case) to its canonical
    /// id and the set of regions it is offered in. The region set may be empty if the
    /// API omitted region info for a valid id; only a total absence of the id is an
    /// error. Shared by `launch_instance_with_filesystem` and the `find` command.
    pub async fn resolve_image_id(&self, id: &str) -> Result<(String, Vec<String>)> {
        let images = self.list_images().await?;
        let target = canonical_id(id);
        let matches: Vec<&Image> = images
            .iter()
            .filter(|i| canonical_id(&i.id) == target)
            .collect();
        let canonical = matches
            .first()
            .ok_or_else(|| {
                anyhow!(
                    "Image id '{}' not found. Run `lambda images` to see valid ids.",
                    id
                )
            })?
            .id
            .clone();
        let mut regions: Vec<String> = matches
            .iter()
            .filter_map(|i| i.region.as_ref().and_then(|r| r.name.clone()))
            .collect();
        regions.sort();
        regions.dedup();
        Ok((canonical, regions))
    }

    /// Validate that an image family exists, erroring with the available families.
    async fn validate_image_family(&self, family: &str) -> Result<()> {
        let images = self.list_images().await?;
        let mut families: Vec<&str> = images.iter().filter_map(|i| i.family.as_deref()).collect();
        families.sort();
        families.dedup();
        if families.contains(&family) {
            Ok(())
        } else {
            Err(anyhow!(
                "Image family '{}' not found. Available families: {}",
                family,
                families.join(", ")
            ))
        }
    }

    /// List all available base images (GET /images)
    pub async fn list_images(&self) -> Result<Vec<Image>> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/images", API_BASE_URL);
        let response = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to fetch images")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to list images: {}", error_msg));
        }

        let response: ApiResponse<Vec<Image>> = response
            .json()
            .await
            .context("Failed to parse images response")?;

        Ok(response.data)
    }

    /// List all filesystems
    pub async fn list_filesystems(&self) -> Result<Vec<Filesystem>> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/file-systems", API_BASE_URL);
        let response = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to fetch filesystems")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to list filesystems: {}", error_msg));
        }

        let response: ApiResponse<Vec<Filesystem>> = response
            .json()
            .await
            .context("Failed to parse filesystems response")?;

        Ok(response.data)
    }

    /// Create a new filesystem
    pub async fn create_filesystem(&self, name: &str, region: &str) -> Result<Filesystem> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/file-systems", API_BASE_URL);
        let payload = serde_json::json!({
            "name": name,
            "region_name": region
        });

        let response = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .json(&payload)
            .send()
            .await
            .context("Failed to create filesystem")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to create filesystem: {}", error_msg));
        }

        let response: ApiResponse<Filesystem> = response
            .json()
            .await
            .context("Failed to parse create filesystem response")?;

        Ok(response.data)
    }

    /// Delete a filesystem
    pub async fn delete_filesystem(&self, filesystem_id: &str) -> Result<()> {
        let api_key = self.get_api_key()?;
        let url = format!("{}/file-systems/{}", API_BASE_URL, filesystem_id);

        let response = self
            .client
            .delete(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .send()
            .await
            .context("Failed to delete filesystem")?;

        if !response.status().is_success() {
            let error_msg = Self::parse_error_response(response).await;
            return Err(anyhow!("Failed to delete filesystem: {}", error_msg));
        }

        Ok(())
    }

    async fn parse_error_response(response: reqwest::Response) -> String {
        response
            .json::<ApiErrorResponse>()
            .await
            .map(|e| e.error.message)
            .unwrap_or_else(|_| "Unknown error".to_string())
    }
}

#[derive(Debug, Clone)]
pub struct LaunchResult {
    pub instance_id: String,
    pub region: String,
}

/// Execute a shell command to retrieve the API key.
fn execute_api_key_command(command: &str) -> Result<String> {
    use std::process::Command;

    let output = if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", command]).output()
    } else {
        Command::new("sh").args(["-c", command]).output()
    };

    match output {
        Ok(output) => {
            if output.status.success() {
                let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if key.is_empty() {
                    Err(LambdaError::ApiKeyCommandFailed(
                        "Command returned empty output".to_string(),
                    )
                    .into())
                } else {
                    Ok(key)
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(
                    LambdaError::ApiKeyCommandFailed(format!("Command failed: {}", stderr.trim()))
                        .into(),
                )
            }
        }
        Err(e) => Err(LambdaError::ApiKeyCommandFailed(format!(
            "Failed to execute command: {}",
            e
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lambda_error_messages() {
        assert_eq!(
            LambdaError::ApiKeyNotSet.to_string(),
            "API key not set. Set LAMBDA_API_KEY or LAMBDA_API_KEY_COMMAND environment variable"
        );
        assert_eq!(
            LambdaError::InstanceTypeNotFound("gpu_1x_a100".to_string()).to_string(),
            "Instance type 'gpu_1x_a100' not found"
        );
    }

    #[test]
    fn test_api_base_url() {
        assert_eq!(API_BASE_URL, "https://cloud.lambdalabs.com/api/v1");
    }

    #[test]
    fn test_image_ref_smart_detects_id_vs_family() {
        // Real ids are UUIDs (hyphenated) or bare 32-hex; families are slugs.
        for id in [
            "f9ba07bd-c60b-4e08-ab29-5d9be6bd62d0",
            "f525e0fb0d234f37b765c6aa55bc459a",
        ] {
            assert!(
                matches!(ImageRef::smart(id), ImageRef::Id(_)),
                "{id} should be an id"
            );
        }
        for family in [
            "lambda-stack-24-04",
            "gpu-base-22-04",
            "ubuntu-24-04",
            "lambda-stack-legacy-22-04",
        ] {
            assert!(
                matches!(ImageRef::smart(family), ImageRef::Family(_)),
                "{family} should be a family"
            );
        }
    }

    #[test]
    fn test_version_key_orders_numerically() {
        // Purely numeric ordering, so 24.4.10 is newer than 24.4.9 (lexical sort
        // would get this wrong) and build suffixes are compared as numbers.
        assert!(version_key("24.4.10-2141") > version_key("24.4.9-9999"));
        assert!(version_key("24.4.4-2141") > version_key("24.4.3-1722"));
        assert!(version_key("22.4.5-20250702") > version_key("22.4.5-20250626"));
        // Non-numeric / empty inputs are handled without panicking.
        assert_eq!(version_key("-"), Vec::<u64>::new());
        assert!(version_key("1.0") > version_key("-"));
    }

    #[test]
    fn test_group_image_builds_dedups_by_family_arch() {
        let img = |id: &str, family: Option<&str>, version: &str, arch: &str, region: &str| Image {
            id: id.to_string(),
            name: None,
            family: family.map(String::from),
            version: Some(version.to_string()),
            architecture: Some(arch.to_string()),
            description: None,
            region: Some(RegionInfo {
                name: Some(region.to_string()),
            }),
        };
        let images = vec![
            // Regions deliberately out of order and duplicated across the two x86_64
            // rows; the newest version is the 3rd row, not the last inserted.
            img("a", Some("ls-24"), "24.4.4-2141", "x86_64", "us-west-1"),
            img("b", Some("ls-24"), "24.4.3-1722", "x86_64", "us-east-1"),
            img("c", Some("ls-24"), "24.4.4-2141", "x86_64", "us-west-1"),
            img("d", Some("ls-24"), "24.4.4-2141", "arm64", "us-east-3"),
            // A null-family image buckets under "-" and must not merge with ls-24.
            img("e", None, "1.0", "x86_64", "eu-west-1"),
        ];
        let builds = group_image_builds(images);
        // (ls-24, x86_64) and (ls-24, arm64) => two builds; the family-less image
        // is excluded (not launchable via a family).
        assert_eq!(builds.len(), 2);

        let x86 = builds
            .iter()
            .find(|b| b.family == "ls-24" && b.arch == "x86_64")
            .unwrap();
        assert_eq!(x86.latest_version, "24.4.4-2141");
        // Sorted and de-duplicated despite out-of-order, duplicated inputs.
        assert_eq!(x86.regions, vec!["us-east-1", "us-west-1"]);
        assert_eq!(x86.regions_summary(), "us-east-1, us-west-1");

        // A family-less image is never presented as a launchable "-" family.
        assert!(builds.iter().all(|b| b.family != "-"));
    }
}
