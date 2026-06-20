use crate::case;
use crate::utils::*;

use s3s_test::Result;
use s3s_test::TestFixture;
use s3s_test::TestSuite;
use s3s_test::tcx::TestContext;

use std::future::Future;
use std::sync::Arc;

use aws_sdk_s3::primitives::SdkBody;
use aws_sdk_s3::types::BucketVersioningStatus;
use aws_sdk_s3::types::Tag;
use aws_sdk_s3::types::Tagging;
use aws_sdk_s3::types::VersioningConfiguration;

pub fn register(tcx: &mut TestContext) {
    case!(tcx, XmlCompat, XmlCompatFixture, test_unknown_element_in_versioning);
    case!(tcx, XmlCompat, XmlCompatFixture, test_unknown_element_in_tagging);
}

struct XmlCompat {
    s3: aws_sdk_s3::Client,
}

impl TestSuite for XmlCompat {
    async fn setup() -> Result<Self> {
        let sdk_conf = aws_config::from_env().load().await;
        let s3 = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&sdk_conf)
                .force_path_style(true) // FIXME: remove force_path_style
                .build(),
        );
        Ok(Self { s3 })
    }
}

struct XmlCompatFixture {
    s3: aws_sdk_s3::Client,
    bucket: String,
}

impl TestFixture<XmlCompat> for XmlCompatFixture {
    fn setup(suite: Arc<XmlCompat>) -> impl Future<Output = Result<Self>> + Send + 'static {
        let s3 = suite.s3.clone();
        async move {
            let bucket = "test-xml-forward-compat".to_owned();
            delete_bucket_loose(&s3, &bucket).await?;
            create_bucket(&s3, &bucket).await?;
            Ok(Self { s3, bucket })
        }
    }
}

impl XmlCompatFixture {
    /// Send `PutBucketVersioning` with an extra unknown XML element.
    ///
    /// A forward-compatible server must accept and ignore the unknown element,
    /// as specified by the S3 protocol. This mirrors Go's `encoding/xml`
    /// behavior where unknown fields are silently discarded.
    async fn test_unknown_element_in_versioning(self: Arc<Self>) -> Result {
        // Includes a hypothetical future element <FutureExtension>.
        const XML: &[u8] = b"<VersioningConfiguration \
            xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
            <Status>Enabled</Status>\
            <FutureExtension>some-future-value</FutureExtension>\
        </VersioningConfiguration>";

        self.s3
            .put_bucket_versioning()
            .bucket(&self.bucket)
            .versioning_configuration(
                VersioningConfiguration::builder()
                    .status(BucketVersioningStatus::Enabled)
                    .build(),
            )
            .customize()
            .mutate_request(|req| {
                *req.body_mut() = SdkBody::from(XML);
            })
            .send()
            .await?;

        // Confirm that the known element was still applied.
        let resp = self.s3.get_bucket_versioning().bucket(&self.bucket).send().await?;
        assert_eq!(resp.status().map(BucketVersioningStatus::as_str), Some("Enabled"));

        Ok(())
    }

    /// Send `PutBucketTagging` with an unknown XML element inside the tag list.
    ///
    /// A forward-compatible server must skip the unknown element and record the
    /// surrounding known tags.
    async fn test_unknown_element_in_tagging(self: Arc<Self>) -> Result {
        // Includes an unknown <UnknownFutureElement> between two known <Tag>s.
        const XML: &[u8] = b"<Tagging>\
            <TagSet>\
                <Tag><Key>env</Key><Value>test</Value></Tag>\
                <UnknownFutureElement><Data>ignored</Data></UnknownFutureElement>\
                <Tag><Key>team</Key><Value>storage</Value></Tag>\
            </TagSet>\
        </Tagging>";

        // The SDK builder requires a non-empty tagging value to pass validation,
        // but the actual HTTP body is replaced by mutate_request below.
        self.s3
            .put_bucket_tagging()
            .bucket(&self.bucket)
            .tagging(
                Tagging::builder()
                    .tag_set(Tag::builder().key("placeholder").value("placeholder").build()?)
                    .build()?,
            )
            .customize()
            .mutate_request(|req| {
                *req.body_mut() = SdkBody::from(XML);
            })
            .send()
            .await?;

        let resp = self.s3.get_bucket_tagging().bucket(&self.bucket).send().await?;
        let tags = resp.tag_set();
        assert_eq!(tags.len(), 2, "expected 2 known tags, got: {tags:?}");
        assert_eq!(tags[0].key(), "env");
        assert_eq!(tags[1].key(), "team");

        Ok(())
    }
}
