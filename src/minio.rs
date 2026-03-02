use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{Client, config::Region, primitives::ByteStream};

use crate::config::MinioConfig;

pub struct MinioUploader {
    client: Client,
    bucket: String,
    public_endpoint: String,
}

impl MinioUploader {
    pub async fn new(config: &MinioConfig) -> Result<Self> {
        let endpoint = normalized_endpoint(&config.endpoint, config.use_ssl);
        let credentials = Credentials::new(
            &config.access_key,
            &config.secret_key,
            None,
            None,
            "ci-Applet",
        );
        let mut loader = aws_config::defaults(BehaviorVersion::latest())
            .credentials_provider(credentials)
            .region(Region::new(config.region.clone()));
        if !endpoint.is_empty() {
            loader = loader.endpoint_url(endpoint.clone());
        }
        let shared = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&shared);
        builder = builder.force_path_style(true);
        let client = Client::from_conf(builder.build());
        let uploader = Self {
            client,
            bucket: config.bucket.clone(),
            public_endpoint: if endpoint.is_empty() {
                config.endpoint.clone()
            } else {
                endpoint
            },
        };
        uploader.ensure_bucket().await?;
        Ok(uploader)
    }

    pub async fn upload_png(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .content_type("image/png")
            .send()
            .await
            .context("uploading screenshot to MinIO")?;

        let endpoint = self.public_endpoint.trim_end_matches('/');
        let public_url = if endpoint.is_empty() {
            format!("s3://{}/{}", self.bucket, key.trim_start_matches('/'))
        } else {
            format!("{endpoint}/{}/{}", self.bucket, key.trim_start_matches('/'))
        };
        Ok(public_url)
    }

    async fn ensure_bucket(&self) -> Result<()> {
        if self
            .client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }

        match self
            .client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                let err_msg = err.to_string();
                if err_msg.contains("BucketAlreadyOwnedByYou")
                    || err_msg.contains("BucketAlreadyExists")
                {
                    return Ok(());
                }
                Err(err.into())
            }
        }
    }
}

fn normalized_endpoint(raw: &str, use_ssl: bool) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        let scheme = if use_ssl { "https" } else { "http" };
        format!("{scheme}://{raw}")
    }
}
