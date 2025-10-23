use crate::config::Config;
use crate::error::{Error, Result};
use log::{debug, info};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

pub struct Client {
    config: Config,
    http: HttpClient,
}

/// Build platform enum matching the backend schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildPlatform {
    Windows,
    Macos,
    Linux,
    Android,
    IosNative,
    IosSimulator,
    Xbox,
    Playstation,
}

/// Deletion policy enum for auto-delete functionality
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionPolicy {
    LeastRecent,
    Oldest,
}

impl DeletionPolicy {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            DeletionPolicy::LeastRecent => "least_recent",
            DeletionPolicy::Oldest => "oldest",
        }
    }
}

impl std::str::FromStr for DeletionPolicy {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "least_recent" | "least-recent" => Ok(DeletionPolicy::LeastRecent),
            "oldest" => Ok(DeletionPolicy::Oldest),
            _ => Err(Error::ConfigError(format!(
                "Invalid deletion policy: '{s}'. Valid policies are: least_recent, oldest"
            ))),
        }
    }
}

impl BuildPlatform {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            BuildPlatform::Windows => "windows",
            BuildPlatform::Macos => "macos",
            BuildPlatform::Linux => "linux",
            BuildPlatform::Android => "android",
            BuildPlatform::IosNative => "ios-native",
            BuildPlatform::IosSimulator => "ios-simulator",
            BuildPlatform::Xbox => "xbox",
            BuildPlatform::Playstation => "playstation",
        }
    }
}

impl std::str::FromStr for BuildPlatform {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "windows" => Ok(BuildPlatform::Windows),
            "macos" => Ok(BuildPlatform::Macos),
            "linux" => Ok(BuildPlatform::Linux),
            "android" => Ok(BuildPlatform::Android),
            "ios-native" => Ok(BuildPlatform::IosNative),
            "ios-simulator" => Ok(BuildPlatform::IosSimulator),
            "xbox" => Ok(BuildPlatform::Xbox),
            "playstation" => Ok(BuildPlatform::Playstation),
            _ => Err(Error::ConfigError(format!(
                "Invalid platform: '{s}'. Valid platforms are: windows, macos, linux, android, ios-native, ios-simulator, xbox, playstation"
            ))),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct UploadRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub file_name: String,
    pub file_size: u64,
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_delete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_timeout: Option<u32>,
}

/// Response from the server for a single-part upload request
#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct SinglePartUploadResponse {
    pub build_id: String,
    pub upload_url: String,
    pub object_key: String,
}

/// Request for initiating a multipart upload
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct InitiateMultipartUploadRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub file_name: String,
    pub file_size: u64,
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_delete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_timeout: Option<u32>,
}

/// Response from the server for a multipart upload request
#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct MultipartUploadResponse {
    pub build_id: String,
    pub upload_id: String,
    pub object_key: String,
    pub total_parts: usize,
    pub part_size: usize,
}

/// Request to get presigned URLs for specific parts
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct GetPresignedUrlsRequest {
    pub upload_id: String,
    pub object_key: String,
    pub part_numbers: Vec<u64>,
}

/// Response with presigned URLs for parts
#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct GetPresignedUrlsResponse {
    pub presigned_urls: Vec<PresignedUrlPart>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct PresignedUrlPart {
    pub part_number: u64,
    pub url: String,
}

/// Uploaded part metadata
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct UploadedPart {
    pub part_number: u64,
    pub etag: String,
}

/// Request to complete multipart upload
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CompleteMultipartUploadRequest {
    pub build_id: String,
    pub upload_id: String,
    pub object_key: String,
    pub parts: Vec<UploadedPart>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CompleteRequest {
    pub build_id: String,
}

impl Client {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            http: HttpClient::new(),
            config,
        }
    }

    /// Request a presigned URL for single-part upload
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or if the server returns a non-success status code.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_upload_url(
        &self,
        name: &str,
        filename: &str,
        size: u64,
        platform: &str,
        description: Option<String>,
        upload_timeout: Option<u32>,
        auto_delete: bool,
        deletion_policy: Option<String>,
    ) -> Result<SinglePartUploadResponse> {
        let url = format!("{}/upload", self.config.base_upload_url());
        debug!("Requesting upload URL from: {url}");

        let request = UploadRequest {
            name: name.to_string(),
            description,
            file_name: filename.to_string(),
            file_size: size,
            platform: platform.to_string(),
            upload_timeout,
            auto_delete: Some(auto_delete),
            deletion_policy,
        };

        let response = self
            .http
            .post(&url)
            .header("x-api-key", self.config.token.clone())
            .json(&request)
            .send()
            .await?;

        info!("Received response with status: {response:?}");

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiError(format!("Status {status}: {body}")));
        }

        // Get the response body as text first to log it
        let body = response.text().await?;
        debug!("Response body: {body}");

        // Try to parse it
        let upload_response: SinglePartUploadResponse =
            serde_json::from_str(&body).map_err(|e| {
                Error::ApiError(format!("Failed to parse response: {e}. Body was: {body}"))
            })?;

        debug!(
            "Received upload URL for build: {} (object: {})",
            upload_response.build_id, upload_response.object_key
        );

        Ok(upload_response)
    }

    /// Upload file to presigned URL
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or if the server returns a non-success status code.
    pub async fn upload_to_presigned_url(&self, url: &str, data: Vec<u8>) -> Result<()> {
        info!("Uploading {} bytes to presigned URL", data.len());

        let response = self
            .http
            .put(url)
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::UploadError(format!("Status {status}: {body}")));
        }

        info!("Upload successful");
        Ok(())
    }

    /// Notify backend that upload is complete
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or if the server returns a non-success status code.
    pub async fn complete_upload(&self, build_id: &str) -> Result<()> {
        let url = format!("{}/upload/complete", self.config.base_upload_url());
        debug!("Completing upload for build: {build_id}");

        let request = CompleteRequest {
            build_id: build_id.to_string(),
        };

        let response = self
            .http
            .post(&url)
            .header("x-api-key", self.config.token.clone())
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiError(format!(
                "Complete failed - Status {status}: {body}"
            )));
        }

        info!("Upload completed successfully");
        Ok(())
    }

    /// Initiate a multipart upload
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or if the server returns a non-success status code.
    #[allow(clippy::too_many_arguments)]
    pub async fn initiate_multipart_upload(
        &self,
        name: &str,
        filename: &str,
        size: u64,
        platform: &str,
        description: Option<String>,
        upload_timeout: Option<u32>,
        auto_delete: bool,
        deletion_policy: Option<String>,
    ) -> Result<MultipartUploadResponse> {
        let url = format!("{}/upload/initiate", self.config.base_upload_url());
        debug!("Initiating multipart upload at: {url}");

        let request = InitiateMultipartUploadRequest {
            name: name.to_string(),
            description,
            file_name: filename.to_string(),
            file_size: size,
            platform: platform.to_string(),
            auto_delete: Some(auto_delete),
            deletion_policy,
            upload_timeout,
        };

        let response = self
            .http
            .post(&url)
            .header("x-api-key", self.config.token.clone())
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiError(format!("Status {status}: {body}")));
        }

        let body = response.text().await?;
        debug!("Initiate response body: {body}");

        let upload_response: MultipartUploadResponse =
            serde_json::from_str(&body).map_err(|e| {
                Error::ApiError(format!("Failed to parse response: {e}. Body was: {body}"))
            })?;

        debug!(
            "Initiated multipart upload - build_id: {}, upload_id: {}, total_parts: {}",
            upload_response.build_id, upload_response.upload_id, upload_response.total_parts
        );

        Ok(upload_response)
    }

    /// Request presigned URLs for specific parts
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or if the server returns a non-success status code.
    pub async fn request_part_urls(
        &self,
        upload_id: &str,
        object_key: &str,
        part_numbers: Vec<u64>,
    ) -> Result<GetPresignedUrlsResponse> {
        let url = format!("{}/upload/parts", self.config.base_upload_url());
        debug!(
            "Requesting presigned URLs for {} parts at: {url}",
            part_numbers.len()
        );

        let request = GetPresignedUrlsRequest {
            upload_id: upload_id.to_string(),
            object_key: object_key.to_string(),
            part_numbers,
        };

        let response = self.http.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiError(format!("Status {status}: {body}")));
        }

        let urls_response: GetPresignedUrlsResponse = response.json().await?;
        debug!(
            "Received {} presigned URLs",
            urls_response.presigned_urls.len()
        );

        Ok(urls_response)
    }

    /// Upload a part to a presigned URL and return the `ETag`
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or if the server returns a non-success status code.
    pub async fn upload_part(&self, url: &str, data: Vec<u8>) -> Result<String> {
        let response = self
            .http
            .put(url)
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::UploadError(format!("Status {status}: {body}")));
        }

        // Extract ETag from response headers
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::UploadError("Missing ETag in response".to_string()))?
            .to_string();

        Ok(etag)
    }

    /// Complete a multipart upload
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or if the server returns a non-success status code.
    pub async fn complete_multipart_upload(
        &self,
        build_id: &str,
        upload_id: &str,
        object_key: &str,
        parts: Vec<UploadedPart>,
    ) -> Result<()> {
        let url = format!("{}/upload/complete", self.config.base_upload_url());
        debug!("Completing multipart upload for build: {build_id}");

        let request = CompleteMultipartUploadRequest {
            build_id: build_id.to_string(),
            upload_id: upload_id.to_string(),
            object_key: object_key.to_string(),
            parts,
        };

        let response = self
            .http
            .post(&url)
            .header("x-api-key", self.config.token.clone())
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiError(format!(
                "Complete multipart failed - Status {status}: {body}"
            )));
        }

        info!("Multipart upload completed successfully");
        Ok(())
    }
}
