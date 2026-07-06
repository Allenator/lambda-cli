use anyhow::Result;
use lambda_cli::api::{
    group_image_builds, version_key, Filesystem, Image, ImageRef, Instance, InstanceTypeData,
    LambdaClient,
};
use lambda_cli::notify::{InstanceReadyMessage, Notifier, NotifyConfig};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::serde::Deserialize;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use std::sync::Arc;
use std::time::Duration;

/// Lambda MCP Server
#[derive(Clone)]
struct LambdaService {
    client: Arc<LambdaClient>,
    notify_config: Option<NotifyConfig>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl LambdaService {
    fn new(lazy: bool) -> Result<Self> {
        dotenv::dotenv().ok();
        let client = LambdaClient::from_env_with_options(lazy)?;
        let notify_config = NotifyConfig::from_env();

        // Debug: log notification config status
        if let Some(ref config) = notify_config {
            eprintln!(
                "[lambda-mcp] Notifications configured for: {}",
                config.configured_channels().join(", ")
            );
        } else {
            eprintln!("[lambda-mcp] No notification channels configured");
        }

        Ok(Self {
            client: Arc::new(client),
            notify_config,
            tool_router: Self::tool_router(),
        })
    }

    fn format_instance_types(types: &[InstanceTypeData]) -> String {
        let mut output = String::from("Available GPU Instance Types:\n\n");
        for t in types {
            let price = t.price_cents_per_hour as f64 / 100.0;
            let availability = if t.regions_available.is_empty() {
                "No availability".to_string()
            } else {
                t.regions_available.join(", ")
            };
            output.push_str(&format!(
                "• {} - {}\n  Price: ${:.2}/hr | vCPUs: {} | RAM: {} GiB | Storage: {} GiB\n  Regions: {}\n\n",
                t.name, t.description, price, t.vcpus, t.memory_gib, t.storage_gib, availability
            ));
        }
        output
    }

    fn format_instances(instances: &[Instance]) -> String {
        if instances.is_empty() {
            return "No running instances.".to_string();
        }
        let mut output = String::from("Running Instances:\n\n");
        for inst in instances {
            let id = inst.id.as_deref().unwrap_or("N/A");
            let name = inst.name.as_deref().unwrap_or("-");
            let status = inst.status.as_deref().unwrap_or("unknown");
            let ip = inst.ip.as_deref().unwrap_or("N/A");
            let inst_type = inst
                .instance_type
                .as_ref()
                .and_then(|t| t.name.as_deref())
                .unwrap_or("N/A");
            let region = inst
                .region
                .as_ref()
                .and_then(|r| r.name.as_deref())
                .unwrap_or("N/A");
            let ssh_keys = inst
                .ssh_key_names
                .as_ref()
                .map(|k| k.join(", "))
                .unwrap_or_else(|| "N/A".to_string());

            output.push_str(&format!(
                "• ID: {}\n  Name: {} | Type: {} | Region: {}\n  Status: {} | IP: {}\n  SSH Keys: {}\n\n",
                id, name, inst_type, region, status, ip, ssh_keys
            ));
        }
        output
    }

    fn format_filesystems(filesystems: &[Filesystem]) -> String {
        if filesystems.is_empty() {
            return "No filesystems.".to_string();
        }
        let mut output = String::from("Filesystems:\n\n");
        for fs in filesystems {
            let bytes_str = if fs.bytes_used > 1_000_000_000 {
                format!("{:.2} GB", fs.bytes_used as f64 / 1_000_000_000.0)
            } else if fs.bytes_used > 1_000_000 {
                format!("{:.2} MB", fs.bytes_used as f64 / 1_000_000.0)
            } else if fs.bytes_used > 1_000 {
                format!("{:.2} KB", fs.bytes_used as f64 / 1_000.0)
            } else {
                format!("{} B", fs.bytes_used)
            };

            output.push_str(&format!(
                "• {} ({})\n  ID: {}\n  Mount: {}\n  Region: {}\n  In Use: {} | Size: {}\n\n",
                fs.name,
                fs.created,
                fs.id,
                fs.mount_point,
                fs.region.name,
                if fs.is_in_use { "Yes" } else { "No" },
                bytes_str
            ));
        }
        output
    }

    /// Grouped image view (default): one line per family+arch with the newest build
    /// and where it's offered. Mirrors `lambda images` so CLI and MCP granularity
    /// match. `family` is the value to pass to start_instance (region-agnostic).
    fn format_image_builds(images: Vec<Image>) -> String {
        let builds = group_image_builds(images);
        if builds.is_empty() {
            return "No images.".to_string();
        }
        let mut output = String::from(
            "Available base images, grouped by family (launch start_instance with a family — \
             region-agnostic, gets the newest build). Pass `family` to this tool to list the \
             individual per-region image ids for a specific build.\n\n",
        );
        for b in builds {
            // `regions_summary` collapses long region lists to a count (shared with
            // the CLI grouped view) so the response stays compact.
            output.push_str(&format!(
                "• family: {} | latest: {} | arch: {} | regions: {}\n",
                b.family,
                b.latest_version,
                b.arch,
                b.regions_summary()
            ));
        }
        output
    }

    /// Drill-down view: every image id/region for one family (the `--all` equivalent,
    /// scoped so it stays small). Use an id here to launch an exact build.
    fn format_images_in_family(images: &[Image], family: &str) -> String {
        let mut matches: Vec<&Image> = images
            .iter()
            .filter(|i| i.family.as_deref() == Some(family))
            .collect();
        if matches.is_empty() {
            return format!(
                "No images in family '{}'. Call list_images with no family to see available families.",
                family
            );
        }
        // Newest version first, then by region name; key computed once per image.
        matches.sort_by_cached_key(|i| {
            (
                std::cmp::Reverse(version_key(i.version.as_deref().unwrap_or(""))),
                i.region.as_ref().and_then(|r| r.name.clone()),
            )
        });
        let mut output = format!(
            "Images in family '{}' (launch start_instance with one of these ids for an exact build):\n\n",
            family
        );
        for img in matches {
            let region = img
                .region
                .as_ref()
                .and_then(|r| r.name.as_deref())
                .unwrap_or("-");
            output.push_str(&format!(
                "• id: {} | name: {} | version: {} | arch: {} | region: {}\n",
                img.id,
                img.name.as_deref().unwrap_or("-"),
                img.version.as_deref().unwrap_or("-"),
                img.architecture.as_deref().unwrap_or("-"),
                region
            ));
        }
        output
    }
}

// Tool parameter types
#[derive(Debug, Deserialize, JsonSchema)]
struct StartInstanceParams {
    /// GPU instance type (e.g., gpu_1x_h100, gpu_1x_a100)
    gpu: String,
    /// SSH key name to use for the instance
    ssh_key: String,
    /// Optional name for the instance
    name: Option<String>,
    /// Optional region to launch in (auto-selects if not specified)
    region: Option<String>,
    /// Optional base image to launch on: either an image id or a family name
    /// (auto-detected). Use the list_images tool to discover valid values.
    /// Defaults to Lambda Stack if omitted.
    image: Option<String>,
    /// Optional filesystem name to attach (must be in the same region)
    filesystem: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StopInstanceParams {
    /// Instance ID to terminate
    instance_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CheckAvailabilityParams {
    /// GPU instance type to check (e.g., gpu_1x_h100)
    gpu: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateFilesystemParams {
    /// Name for the filesystem
    name: String,
    /// Region to create the filesystem in (e.g., us-east-1)
    region: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteFilesystemParams {
    /// Filesystem ID to delete
    filesystem_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListImagesParams {
    /// Optional family name to drill into: lists the individual per-region image
    /// ids for that family. Omit for the grouped, one-per-family overview.
    family: Option<String>,
}

#[tool_router]
impl LambdaService {
    #[tool(
        description = "List all available GPU instance types with pricing, specs, and current availability"
    )]
    async fn list_gpu_types(&self) -> Result<CallToolResult, McpError> {
        let types = self
            .client
            .list_instance_types()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(
            Self::format_instance_types(&types),
        )]))
    }

    #[tool(
        description = "Launch a new GPU instance. Returns instance ID and connection details. Optionally attach a filesystem (must be in the same region) or select a base image via the image param, which accepts either an image id or a family name (defaults to Lambda Stack; use list_images to discover valid values). If notification env vars are configured, will auto-notify when instance is SSH-able."
    )]
    async fn start_instance(
        &self,
        Parameters(params): Parameters<StartInstanceParams>,
    ) -> Result<CallToolResult, McpError> {
        // Trim and treat empty/whitespace as absent — agents often send "" or a
        // value with stray spaces for an optional field; those would otherwise be
        // classified as a (bogus) family and fail validation.
        let image = params
            .image
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ImageRef::smart);

        let result = self
            .client
            .launch_instance_with_filesystem(
                &params.gpu,
                &params.ssh_key,
                params.name.as_deref(),
                params.region.as_deref(),
                image,
                params.filesystem.as_deref(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let fs_info = params
            .filesystem
            .as_ref()
            .map(|f| format!("\nFilesystem: {} (mounted at /lambda/nfs/{})", f, f))
            .unwrap_or_default();

        // Spawn background task to notify when instance is ready
        let notify_status = if let Some(ref config) = self.notify_config {
            let channels = config.configured_channels().join(", ");
            let client = Arc::clone(&self.client);
            let notifier = Notifier::new(config.clone());
            let instance_id = result.instance_id.clone();
            let instance_name = params.name.clone();
            let gpu_type = params.gpu.clone();
            let region = result.region.clone();

            tokio::spawn(async move {
                poll_and_notify(
                    client,
                    notifier,
                    instance_id,
                    instance_name,
                    gpu_type,
                    region,
                )
                .await;
            });

            format!("\n\nNotifications enabled for: {}. You will be notified when the instance is SSH-able.", channels)
        } else {
            String::new()
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Instance launched successfully!\n\nInstance ID: {}\nRegion: {}{}\n\nNote: Instance may take a few minutes to become active. Use 'list_running_instances' to check status.{}",
            result.instance_id, result.region, fs_info, notify_status
        ))]))
    }

    #[tool(description = "Terminate a running GPU instance")]
    async fn stop_instance(
        &self,
        Parameters(params): Parameters<StopInstanceParams>,
    ) -> Result<CallToolResult, McpError> {
        self.client
            .terminate_instance(&params.instance_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Instance {} terminated successfully.",
            params.instance_id
        ))]))
    }

    #[tool(
        description = "List all currently running GPU instances with their status and connection details"
    )]
    async fn list_running_instances(&self) -> Result<CallToolResult, McpError> {
        let instances = self
            .client
            .list_running_instances()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(
            Self::format_instances(&instances),
        )]))
    }

    #[tool(
        description = "Check if a specific GPU type is currently available. Returns list of regions with availability."
    )]
    async fn check_availability(
        &self,
        Parameters(params): Parameters<CheckAvailabilityParams>,
    ) -> Result<CallToolResult, McpError> {
        let regions = self
            .client
            .check_availability(&params.gpu)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let message = if regions.is_empty() {
            format!(
                "GPU type '{}' is currently NOT available in any region.",
                params.gpu
            )
        } else {
            format!(
                "GPU type '{}' is available in: {}",
                params.gpu,
                regions.join(", ")
            )
        };

        Ok(CallToolResult::success(vec![Content::text(message)]))
    }

    #[tool(
        description = "List all filesystems (persistent storage). Filesystems can be attached to instances at launch time."
    )]
    async fn list_filesystems(&self) -> Result<CallToolResult, McpError> {
        let filesystems = self
            .client
            .list_filesystems()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(
            Self::format_filesystems(&filesystems),
        )]))
    }

    #[tool(
        description = "List available base images, grouped by family (one entry per family with its newest build and regions). Launch by passing a family to start_instance's image param (recommended, region-agnostic). Pass a `family` to this tool to drill into that family's individual per-region image ids for launching an exact build."
    )]
    async fn list_images(
        &self,
        Parameters(params): Parameters<ListImagesParams>,
    ) -> Result<CallToolResult, McpError> {
        let images = self
            .client
            .list_images()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Treat an empty/whitespace family as absent — agents commonly fill an
        // optional string field with "" and expect the default (grouped) view.
        let family = params
            .family
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty());
        let text = match family {
            Some(family) => Self::format_images_in_family(&images, family),
            None => Self::format_image_builds(images),
        };

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        description = "Create a new filesystem (persistent storage). Filesystems must be in the same region as instances they attach to."
    )]
    async fn create_filesystem(
        &self,
        Parameters(params): Parameters<CreateFilesystemParams>,
    ) -> Result<CallToolResult, McpError> {
        let fs = self
            .client
            .create_filesystem(&params.name, &params.region)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Filesystem created successfully!\n\nName: {}\nID: {}\nRegion: {}\nMount point: {}",
            fs.name, fs.id, fs.region.name, fs.mount_point
        ))]))
    }

    #[tool(description = "Delete a filesystem. The filesystem must not be in use by any instance.")]
    async fn delete_filesystem(
        &self,
        Parameters(params): Parameters<DeleteFilesystemParams>,
    ) -> Result<CallToolResult, McpError> {
        self.client
            .delete_filesystem(&params.filesystem_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Filesystem {} deleted successfully.",
            params.filesystem_id
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for LambdaService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Lambda GPU instance management. Use these tools to list available GPU types, \
                 launch instances, check running instances, terminate instances, and manage \
                 persistent filesystems. Filesystems can be attached to instances at launch time \
                 for persistent storage. Requires LAMBDA_API_KEY environment variable to be set."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Background task to poll for instance readiness and send notifications
async fn poll_and_notify(
    client: Arc<LambdaClient>,
    notifier: Notifier,
    instance_id: String,
    instance_name: Option<String>,
    gpu_type: String,
    region: String,
) {
    let max_wait = Duration::from_secs(600); // 10 minutes max
    let poll_interval = Duration::from_secs(10);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > max_wait {
            eprintln!(
                "[notify] Timeout waiting for instance {} to become active",
                instance_id
            );
            return;
        }

        tokio::time::sleep(poll_interval).await;

        match client.get_instance(&instance_id).await {
            Ok(instance) => {
                let status = instance.status.as_deref().unwrap_or("unknown");

                if status == "terminated" || status == "unhealthy" {
                    eprintln!(
                        "[notify] Instance {} entered {} state, stopping notifications",
                        instance_id, status
                    );
                    return;
                }

                // Notify when IP is available (don't wait for "active" status)
                if let Some(ip) = instance.ip {
                    let msg = InstanceReadyMessage {
                        instance_id: instance_id.clone(),
                        instance_name,
                        ip,
                        gpu_type,
                        region,
                    };

                    let results = notifier.send_all(&msg).await;
                    for (channel, result) in results {
                        match result {
                            Ok(()) => eprintln!(
                                "[notify] {} notification sent for {}",
                                channel, instance_id
                            ),
                            Err(e) => {
                                eprintln!("[notify] {} notification failed: {}", channel, e)
                            }
                        }
                    }
                    return;
                }
                // No IP yet, continue polling
            }
            Err(e) => {
                eprintln!("[notify] Error checking instance {}: {}", instance_id, e);
                // Continue polling on transient errors
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    // Lazy loading is the default for MCP servers; use --eager to load API key at startup
    let lazy = !args.iter().any(|arg| arg == "--eager");

    // Initialize the service
    let service = match LambdaService::new(lazy) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to initialize Lambda service: {}", e);
            eprintln!("Make sure LAMBDA_API_KEY environment variable is set.");
            std::process::exit(1);
        }
    };

    // Run the MCP server over stdio
    let server = service.serve(rmcp::transport::io::stdio()).await?;

    // Wait for the server to finish
    server.waiting().await?;

    Ok(())
}
