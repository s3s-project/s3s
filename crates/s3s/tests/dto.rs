use s3s::dto::*;

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
