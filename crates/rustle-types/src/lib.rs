//! DTOs shared by the Rustle server and the WebAssembly UI.
//! No async runtime and nothing that cannot compile to wasm32.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResp {
    pub username: String,
    /// CSRF token to echo in the `x-csrf-token` header on mutating requests.
    pub csrf: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePasswordReq {
    pub current: String,
    pub new: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemStats {
    pub cpu_percent: f32,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
    pub disk_total_gb: f32,
    pub disk_used_gb: f32,
    pub uptime_secs: u64,
    /// Resident set size of the Rustle process in KiB.
    pub rustle_rss_kb: u64,
    pub gateway_ok: bool,
    pub runtime_ok: bool,
    pub degraded_reason: Option<String>,
    pub allocated_mb: u64,
    pub budget_mb: u64,
    pub public_ip_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteDto {
    pub id: String,
    pub name: String,
    pub domain: String,
    pub image_repo: String,
    pub container_port: u16,
    pub ram_limit_mb: u32,
    pub status: String,
    pub zero_downtime: bool,
    pub compat_caps: bool,
    pub registry_id: Option<String>,
    pub current_deployment_id: Option<String>,
    pub ram_used_mb: Option<u32>,
    pub last_deployed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSiteReq {
    pub name: String,
    pub domain: String,
    /// Example: `ghcr.io/owner/app`. Tag or digest is supplied by deploys.
    pub image_repo: String,
    pub ram_limit_mb: u32,
    #[serde(default = "default_port")]
    pub container_port: u16,
    #[serde(default)]
    pub registry_id: Option<String>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// Skip the DNS pointing check (for staging or when DNS is not ready yet).
    #[serde(default)]
    pub skip_dns_check: bool,
    /// Accept an allocation above the RAM budget.
    #[serde(default)]
    pub override_budget: bool,
}

fn default_port() -> u16 {
    3000
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateSiteReq {
    pub ram_limit_mb: Option<u32>,
    pub container_port: Option<u16>,
    pub zero_downtime: Option<bool>,
    pub compat_caps: Option<bool>,
    pub registry_id: Option<Option<String>>,
    pub override_budget: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvListResp {
    /// Values are masked unless `reveal=true` was requested.
    pub vars: Vec<EnvVar>,
    pub masked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutEnvReq {
    pub vars: Vec<EnvVar>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenDto {
    pub id: String,
    pub prefix: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenCreated {
    pub id: String,
    /// Shown exactly once.
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentDto {
    pub id: String,
    pub site_id: String,
    pub image_ref: String,
    pub image_digest: Option<String>,
    pub status: String,
    pub trigger: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployWebhookReq {
    pub image: String,
    #[serde(default)]
    pub site_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployAccepted {
    pub deployment_id: String,
    pub queued: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackReq {
    pub deployment_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CronJobDto {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub schedule: String,
    pub site_id: Option<String>,
    pub argv: Option<Vec<String>>,
    pub enabled: bool,
    pub system_key: Option<String>,
    pub last_status: Option<String>,
    pub last_run_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCronReq {
    pub name: String,
    pub schedule: String,
    pub site_id: String,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateCronReq {
    pub enabled: Option<bool>,
    pub schedule: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronRunDto {
    pub id: i64,
    pub job_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub log_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryDto {
    pub id: String,
    pub name: String,
    pub host: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRegistryReq {
    pub name: String,
    pub username: String,
    /// Personal access token with `read:packages`.
    pub secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackupConfig {
    pub enabled: bool,
    /// `owner/repo` of a PRIVATE repository.
    pub repo: String,
    pub branch: String,
    pub path_prefix: String,
    /// Write-only: never returned by the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub token_set: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRunDto {
    pub id: i64,
    pub ts: i64,
    pub status: String,
    pub bytes: i64,
    pub remote_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub enabled: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SettingsDto {
    pub admin_domain: String,
    pub acme_email: String,
    pub budget_override_mb: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowResp {
    pub workflow_yaml: String,
    pub dockerfile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneResp {
    pub reclaimed_bytes: u64,
    pub images_deleted: u32,
}
