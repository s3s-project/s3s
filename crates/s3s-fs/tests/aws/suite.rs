// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023-2026 The s3s Authors

use s3s::auth::SimpleAuth;
use s3s::header::CONTENT_TYPE;
use s3s::host::SingleDomain;
use s3s::route::S3Route;
use s3s::service::S3ServiceBuilder;
use s3s::validation::NameValidation;
use s3s_fs::FileSystem;

use std::fs;
use std::future::Future;
use std::sync::Arc;

use aws_config::SdkConfig;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::config::Region;
use hyper::Method;
use s3s_test::Result;
use s3s_test::TestFixture;
use s3s_test::TestSuite;
use tracing::debug;

pub(crate) const FS_ROOT: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/s3s-fs-tests-aws");
pub(crate) const DOMAIN_NAME: &str = "localhost:8014";
pub(crate) const REGION: &str = "us-west-2";

// STS AssumeRole route that returns NotImplemented
pub(crate) struct AssumeRoleRoute;

#[async_trait::async_trait]
impl S3Route for AssumeRoleRoute {
    fn is_match(&self, method: &Method, uri: &hyper::Uri, headers: &hyper::HeaderMap, _: &mut hyper::http::Extensions) -> bool {
        if method == Method::POST
            && uri.path() == "/"
            && let Some(val) = headers.get(CONTENT_TYPE)
            && val.as_bytes() == b"application/x-www-form-urlencoded"
        {
            return true;
        }
        false
    }

    async fn call(&self, _req: s3s::S3Request<s3s::Body>) -> s3s::S3Result<s3s::S3Response<s3s::Body>> {
        debug!("AssumeRole called - returning NotImplemented");
        Err(s3s::s3_error!(NotImplemented, "STS operations are not supported by s3s-fs"))
    }
}

fn build_service(relaxed: bool) -> s3s::service::S3Service {
    fs::create_dir_all(FS_ROOT).unwrap();
    let fs = FileSystem::new(FS_ROOT).unwrap();

    // Fake credentials
    let cred = Credentials::for_tests();

    let mut b = S3ServiceBuilder::new(fs);
    b.set_auth(SimpleAuth::from_single(cred.access_key_id(), cred.secret_access_key()));
    b.set_host(SingleDomain::new(DOMAIN_NAME).unwrap());
    b.set_route(AssumeRoleRoute);
    if relaxed {
        b.set_validation(RelaxedNameValidation);
    }
    b.build()
}

fn build_sdk_config(service: s3s::service::S3Service) -> SdkConfig {
    // Fake credentials
    let cred = Credentials::for_tests();

    // Convert to aws http client
    let client = s3s_aws::Client::from(service);

    // Setup aws sdk config
    SdkConfig::builder()
        .credentials_provider(SharedCredentialsProvider::new(cred))
        .http_client(client)
        .region(Region::new(REGION))
        .endpoint_url(format!("http://{DOMAIN_NAME}"))
        .build()
}

pub(crate) struct FsServer {
    sdk_config: SdkConfig,
}

impl TestSuite for FsServer {
    #[tracing::instrument(skip_all)]
    async fn setup() -> Result<Self> {
        let service = build_service(false);
        let sdk_config = build_sdk_config(service);
        Ok(Self { sdk_config })
    }
}

pub(crate) struct FsServerRelaxed {
    sdk_config: SdkConfig,
}

pub(crate) struct RelaxedNameValidation;

impl NameValidation for RelaxedNameValidation {
    fn validate_bucket_name(&self, name: &str) -> bool {
        !name.is_empty()
    }
}

impl TestSuite for FsServerRelaxed {
    #[tracing::instrument(skip_all)]
    async fn setup() -> Result<Self> {
        let service = build_service(true);
        let sdk_config = build_sdk_config(service);
        Ok(Self { sdk_config })
    }
}

macro_rules! define_fixture {
    ($name:ident) => {
        pub(crate) struct $name {
            pub(crate) s3: aws_sdk_s3::Client,
        }

        impl TestFixture<FsServer> for $name {
            fn setup(suite: Arc<FsServer>) -> impl Future<Output = Result<Self>> + Send + 'static {
                let sdk_config = suite.sdk_config.clone();
                async move {
                    Ok(Self {
                        s3: aws_sdk_s3::Client::new(&sdk_config),
                    })
                }
            }
        }
    };
}

define_fixture!(Bucket);
define_fixture!(List);
define_fixture!(Object);
define_fixture!(Multipart);
define_fixture!(Copy);
define_fixture!(Conditional);

pub(crate) struct Essential {
    pub(crate) s3: aws_sdk_s3::Client,
}

impl TestFixture<FsServerRelaxed> for Essential {
    fn setup(suite: Arc<FsServerRelaxed>) -> impl Future<Output = Result<Self>> + Send + 'static {
        let sdk_config = suite.sdk_config.clone();
        async move {
            Ok(Self {
                s3: aws_sdk_s3::Client::new(&sdk_config),
            })
        }
    }
}

pub(crate) struct Sts {
    pub(crate) sts: aws_sdk_sts::Client,
}

impl TestFixture<FsServer> for Sts {
    fn setup(suite: Arc<FsServer>) -> impl Future<Output = Result<Self>> + Send + 'static {
        let sdk_config = suite.sdk_config.clone();
        async move {
            Ok(Self {
                sts: aws_sdk_sts::Client::new(&sdk_config),
            })
        }
    }
}

#[macro_export]
macro_rules! case {
    ($tcx: expr, $s:ident, $x:ident, $c:ident) => {{
        #[allow(clippy::wildcard_imports)]
        use $crate::suite::*;
        let mut suite = $tcx.suite::<$s>(stringify!($s));
        let mut fixture = suite.fixture::<$x>(stringify!($x));
        fixture.case(stringify!($c), $x::$c);
    }};
}

#[tracing::instrument(skip(s3))]
pub(crate) async fn create_bucket(s3: &aws_sdk_s3::Client, bucket: &str) -> Result<()> {
    let location = aws_sdk_s3::types::BucketLocationConstraint::from(REGION);
    let cfg = aws_sdk_s3::types::CreateBucketConfiguration::builder()
        .location_constraint(location)
        .build();

    s3.create_bucket()
        .create_bucket_configuration(cfg)
        .bucket(bucket)
        .send()
        .await?;

    debug!("created bucket: {bucket:?}");
    Ok(())
}

#[tracing::instrument(skip(s3))]
pub(crate) async fn delete_object(s3: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Result<()> {
    s3.delete_object().bucket(bucket).key(key).send().await?;
    Ok(())
}

#[tracing::instrument(skip(s3))]
pub(crate) async fn delete_bucket(s3: &aws_sdk_s3::Client, bucket: &str) -> Result<()> {
    s3.delete_bucket().bucket(bucket).send().await?;
    Ok(())
}

pub(crate) async fn do_multipart_upload(
    s3: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    content: &[u8],
) -> Result<(String, Vec<aws_sdk_s3::types::CompletedPart>)> {
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::types::CompletedPart;

    let upload_id = s3
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await?
        .upload_id
        .ok_or_else(|| s3s_test::Failed::from_string("create_multipart_upload response missing upload_id"))?;

    let ans = s3
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .body(ByteStream::from(content.to_vec()))
        .part_number(1)
        .send()
        .await?;

    let e_tag = ans
        .e_tag
        .ok_or_else(|| s3s_test::Failed::from_string("upload_part returned no ETag"))?;

    let part = CompletedPart::builder().e_tag(e_tag).part_number(1).build();

    Ok((upload_id, vec![part]))
}
