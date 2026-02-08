use s3s::dto::{
    AnalyticsConfiguration, BucketLifecycleConfiguration, GetObjectInput, IntelligentTieringConfiguration,
    InventoryConfiguration, LambdaFunctionConfiguration, ListObjectsV2Input, ListObjectsV2Output, MetadataTableConfiguration,
    MetricsConfiguration, PutObjectOutput, QueueConfiguration, ReplicationConfiguration, RequestPaymentConfiguration,
    TopicConfiguration,
};

#[test]
fn builder() {
    let input = {
        let mut b = GetObjectInput::builder();
        b.set_bucket("hello".to_owned());
        b.set_key("world".to_owned());
        b.build().unwrap()
    };

    assert_eq!(input.bucket, "hello");
    assert_eq!(input.key, "world");
}

#[test]
fn configuration_types_have_default() {
    // Test the two types mentioned in the issue
    let _ = BucketLifecycleConfiguration::default();
    let _ = ReplicationConfiguration::default();

    // Test a few more Configuration types
    let _ = AnalyticsConfiguration::default();
    let _ = IntelligentTieringConfiguration::default();
    let _ = InventoryConfiguration::default();
    let _ = LambdaFunctionConfiguration::default();
    let _ = MetadataTableConfiguration::default();
    let _ = MetricsConfiguration::default();
    let _ = QueueConfiguration::default();
    let _ = RequestPaymentConfiguration::default();
    let _ = TopicConfiguration::default();
}

#[test]
fn configuration_serialization() {
    // Test that Configuration types can be serialized and deserialized
    let config = BucketLifecycleConfiguration::default();
    let json = serde_json::to_string(&config).expect("should serialize");
    let _: BucketLifecycleConfiguration = serde_json::from_str(&json).expect("should deserialize");

    let config = ReplicationConfiguration::default();
    let json = serde_json::to_string(&config).expect("should serialize");
    let _: ReplicationConfiguration = serde_json::from_str(&json).expect("should deserialize");
}

#[test]
fn configuration_types_have_clone() {
    let _ = BucketLifecycleConfiguration::default().clone();
    let _ = ReplicationConfiguration::default().clone();

    let _ = AnalyticsConfiguration::default().clone();
    let _ = IntelligentTieringConfiguration::default().clone();
    let _ = InventoryConfiguration::default().clone();
    let _ = LambdaFunctionConfiguration::default().clone();
    let _ = MetadataTableConfiguration::default().clone();
    let _ = MetricsConfiguration::default().clone();
    let _ = QueueConfiguration::default().clone();
    let _ = RequestPaymentConfiguration::default().clone();
    let _ = TopicConfiguration::default().clone();
}

#[test]
fn operation_types_have_clone() {
    let _ = GetObjectInput::default().clone();
    let _ = ListObjectsV2Input::default().clone();
    let _ = ListObjectsV2Output::default().clone();
    let _ = PutObjectOutput::default().clone();
}
