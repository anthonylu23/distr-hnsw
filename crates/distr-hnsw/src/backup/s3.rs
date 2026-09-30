//! S3-compatible backup target. Backblaze B2 is the reference deployment,
//! MinIO the test double; AWS S3 works unchanged.
//!
//! The adapter keeps the [`super`] layout under an optional key prefix and
//! enforces the same never-overwrite contract as the directory adapter with
//! conditional writes (`If-None-Match: *`). Bucket versioning is required so
//! catalogs and snapshots cannot be silently replaced; Object Lock (governance
//! mode with a default retention equal to the offsite window, DESIGN §11.1) is
//! recommended and reported through [`S3Capabilities`] but not required,
//! because a plain S3-compatible store may not offer it.
//!
//! # Configuration
//!
//! | Variable | Meaning |
//! |---|---|
//! | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | Required. `AWS_SESSION_TOKEN` is honoured when present. |
//! | `AWS_REGION` | Signing region; defaults to `us-east-1`. |
//! | `DISTR_HNSW_S3_ENDPOINT` | Custom endpoint, e.g. `https://s3.us-west-004.backblazeb2.com` or `http://127.0.0.1:9000`. Path-style addressing is used whenever it is set. |
//!
//! Credentials are read from the environment only; shared credential files,
//! profiles, and instance roles are deliberately not consulted so that a
//! backup job's identity is explicit in its unit file.
//!
//! # Required permissions
//!
//! Bucket: `s3:ListBucket`, `s3:GetBucketVersioning`,
//! `s3:GetBucketObjectLockConfiguration`. Objects: `s3:PutObject`,
//! `s3:GetObject`. No delete permission is needed, and the key used by the
//! backup job should not have one. A Backblaze B2 application key needs the
//! `listBuckets`, `listFiles`, `readFiles`, `writeFiles`, and
//! `readBucketRetentions` capabilities. The integration tests additionally
//! create buckets (`s3:CreateBucket`, `s3:PutBucketVersioning`,
//! `s3:PutBucketObjectLockConfiguration`).
//!
//! # Compatibility notes
//!
//! Request and response checksums are limited to operations that require
//! them: some S3-compatible stores reject the `x-amz-checksum-*` headers and
//! `aws-chunked` bodies the SDK otherwise adds, and every copy is verified by
//! reading it back and hashing it anyway.

use std::env;

use aws_sdk_s3::{
    config::{
        http::HttpResponse, BehaviorVersion, Credentials, Region, RequestChecksumCalculation,
        ResponseChecksumValidation,
    },
    error::{DisplayErrorContext, ProvideErrorMetadata, SdkError},
    primitives::ByteStream,
    types::{BucketVersioningStatus, ObjectLockEnabled},
    Client, Config,
};
use serde::Serialize;

use super::{validate_path, BackupError, PutOutcome};

/// Environment variable naming a custom S3 endpoint.
pub const ENDPOINT_ENV: &str = "DISTR_HNSW_S3_ENDPOINT";
const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_PAGE_SIZE: i32 = 1000;

pub(super) fn target_id(bucket: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        format!("s3:{bucket}")
    } else {
        format!("s3:{bucket}/{prefix}")
    }
}

/// Default retention rule of a bucket with Object Lock enabled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct S3DefaultRetention {
    /// `GOVERNANCE` or `COMPLIANCE`.
    pub mode: String,
    pub days: Option<i32>,
    pub years: Option<i32>,
}

/// What the bucket offered when the target was opened.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct S3Capabilities {
    /// Always true for an open target; opening fails closed otherwise.
    pub versioning_enabled: bool,
    pub object_lock_enabled: bool,
    pub default_retention: Option<S3DefaultRetention>,
    /// Non-fatal findings, such as a missing Object Lock configuration.
    pub warnings: Vec<String>,
}

/// One bucket plus an optional key prefix; see the module documentation.
pub struct S3Target {
    client: Client,
    bucket: String,
    prefix: String,
    capabilities: S3Capabilities,
    page_size: i32,
}

fn non_empty(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

/// Build a client from the environment (see the module documentation).
pub fn client_from_env() -> Result<Client, BackupError> {
    let (Some(access_key), Some(secret_key)) = (
        non_empty("AWS_ACCESS_KEY_ID"),
        non_empty("AWS_SECRET_ACCESS_KEY"),
    ) else {
        return Err(BackupError::S3Credentials);
    };
    let credentials = Credentials::new(
        access_key,
        secret_key,
        non_empty("AWS_SESSION_TOKEN"),
        None,
        "environment",
    );
    let region = non_empty("AWS_REGION").unwrap_or_else(|| DEFAULT_REGION.to_owned());
    let mut builder = Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region))
        .credentials_provider(credentials)
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
    if let Some(endpoint) = non_empty(ENDPOINT_ENV) {
        builder = builder.endpoint_url(endpoint).force_path_style(true);
    }
    Ok(Client::from_conf(builder.build()))
}

fn s3_error<E, R>(operation: &str, subject: &str, error: &SdkError<E, R>) -> BackupError
where
    E: std::error::Error + 'static,
    R: std::fmt::Debug,
{
    BackupError::S3(format!(
        "{operation} {subject}: {}",
        DisplayErrorContext(error)
    ))
}

fn status_code<E>(error: &SdkError<E, HttpResponse>) -> Option<u16> {
    match error {
        SdkError::ServiceError(context) => Some(context.raw().status().as_u16()),
        _ => None,
    }
}

fn error_code<E: ProvideErrorMetadata>(error: &SdkError<E, HttpResponse>) -> Option<&str> {
    match error {
        SdkError::ServiceError(context) => context.err().code(),
        _ => None,
    }
}

async fn probe(client: &Client, bucket: &str) -> Result<S3Capabilities, BackupError> {
    let versioning = client
        .get_bucket_versioning()
        .bucket(bucket)
        .send()
        .await
        .map_err(|error| s3_error("GetBucketVersioning", bucket, &error))?;
    let mut capabilities = S3Capabilities {
        versioning_enabled: matches!(versioning.status(), Some(BucketVersioningStatus::Enabled)),
        ..S3Capabilities::default()
    };
    match client
        .get_object_lock_configuration()
        .bucket(bucket)
        .send()
        .await
    {
        Ok(output) => {
            let configuration = output.object_lock_configuration();
            capabilities.object_lock_enabled = configuration
                .and_then(|configuration| configuration.object_lock_enabled())
                == Some(&ObjectLockEnabled::Enabled);
            capabilities.default_retention = configuration
                .and_then(|configuration| configuration.rule())
                .and_then(|rule| rule.default_retention())
                .map(|retention| S3DefaultRetention {
                    mode: retention
                        .mode()
                        .map(|mode| mode.as_str().to_owned())
                        .unwrap_or_default(),
                    days: retention.days(),
                    years: retention.years(),
                });
            if !capabilities.object_lock_enabled {
                capabilities.warnings.push(
                    "Object Lock is not enabled; versioning alone does not stop a privileged \
                     deletion of the backup set"
                        .to_owned(),
                );
            } else if capabilities.default_retention.is_none() {
                capabilities.warnings.push(
                    "Object Lock is enabled without a default retention; new backup objects \
                     are not retained unless a retention period is applied"
                        .to_owned(),
                );
            }
        }
        Err(error) => capabilities.warnings.push(format!(
            "Object Lock configuration could not be read ({}); treating it as absent",
            error_code(&error).unwrap_or("no error code")
        )),
    }
    Ok(capabilities)
}

impl S3Target {
    /// Open `bucket` with a client built from the environment.
    pub async fn open_from_env(bucket: &str, prefix: &str) -> Result<Self, BackupError> {
        Self::open(client_from_env()?, bucket, prefix).await
    }

    /// Open `bucket` with an existing client. Fails closed unless bucket
    /// versioning is `Enabled`; a missing Object Lock configuration is
    /// reported on stderr and in [`S3Target::capabilities`].
    pub async fn open(client: Client, bucket: &str, prefix: &str) -> Result<Self, BackupError> {
        let prefix = prefix.trim_matches('/');
        if !prefix.is_empty() {
            validate_path(prefix)?;
        }
        let capabilities = probe(&client, bucket).await?;
        if !capabilities.versioning_enabled {
            return Err(BackupError::TargetUnversioned(bucket.to_owned()));
        }
        for warning in &capabilities.warnings {
            eprintln!(
                "warning: backup target {}: {warning}",
                target_id(bucket, prefix)
            );
        }
        Ok(Self {
            client,
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            capabilities,
            page_size: DEFAULT_PAGE_SIZE,
        })
    }

    /// Use smaller ListObjectsV2 pages (1..=1000); tests exercise pagination
    /// with this.
    pub fn with_list_page_size(mut self, page_size: i32) -> Self {
        self.page_size = page_size.clamp(1, DEFAULT_PAGE_SIZE);
        self
    }

    pub fn capabilities(&self) -> &S3Capabilities {
        &self.capabilities
    }

    pub fn id(&self) -> String {
        target_id(&self.bucket, &self.prefix)
    }

    fn key(&self, path: &str) -> String {
        if self.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}/{path}", self.prefix)
        }
    }

    fn relative<'a>(&self, key: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            Some(key)
        } else {
            key.strip_prefix(&self.prefix)?.strip_prefix('/')
        }
    }

    /// Conditional write. A precondition failure means the key exists: the
    /// object is read back and compared, so identical bytes are accepted and
    /// different bytes are a [`BackupError::Conflict`].
    pub async fn put_new(&self, path: &str, bytes: &[u8]) -> Result<PutOutcome, BackupError> {
        validate_path(path)?;
        let key = self.key(path);
        let result = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .if_none_match("*")
            .body(ByteStream::from(bytes.to_vec()))
            .send()
            .await;
        match result {
            Ok(_) => Ok(PutOutcome::Created),
            Err(error)
                if status_code(&error) == Some(412)
                    || matches!(
                        error_code(&error),
                        Some("PreconditionFailed" | "ConditionalRequestConflict")
                    ) =>
            {
                match self.get(path).await? {
                    Some(existing) if existing == bytes => Ok(PutOutcome::Identical),
                    Some(_) => Err(BackupError::Conflict(path.to_owned())),
                    None => Err(BackupError::S3(format!(
                        "PutObject {key}: precondition failed but the object could not be read back"
                    ))),
                }
            }
            Err(error) => Err(s3_error("PutObject", &key, &error)),
        }
    }

    /// `None` when the key does not exist (or its current version is a
    /// delete marker).
    pub async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, BackupError> {
        validate_path(path)?;
        let key = self.key(path);
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(output) => {
                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|error| BackupError::S3(format!("GetObject {key}: {error}")))?;
                Ok(Some(body.into_bytes().to_vec()))
            }
            Err(error) if status_code(&error) == Some(404) => Ok(None),
            Err(error)
                if matches!(
                    error.as_service_error(),
                    Some(service) if service.is_no_such_key()
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(s3_error("GetObject", &key, &error)),
        }
    }

    /// Every key under `prefix/`, fully paginated, reported relative to the
    /// configured prefix so paths look exactly like the directory adapter's.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, BackupError> {
        validate_path(prefix)?;
        let key_prefix = format!("{}/", self.key(prefix));
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&key_prefix)
            .into_paginator()
            .page_size(self.page_size)
            .send();
        let mut out = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|error| s3_error("ListObjectsV2", &key_prefix, &error))?;
            for object in page.contents() {
                let Some(relative) = object.key().and_then(|key| self.relative(key)) else {
                    continue;
                };
                // Folder placeholders and unusable names are invisible, as
                // dot-files are to the directory adapter.
                if relative.ends_with('/') || validate_path(relative).is_err() {
                    continue;
                }
                let size = u64::try_from(object.size().unwrap_or(0)).unwrap_or(0);
                out.push((relative.to_owned(), size));
            }
        }
        out.sort();
        Ok(out)
    }
}
