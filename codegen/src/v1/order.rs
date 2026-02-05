pub fn struct_member_order(shape_name: &str) -> Option<&'static [&'static str]> {
    match shape_name {
        "ListObjectsOutput" => Some(LIST_OBJECTS_OUTPUT_ORDER),
        "ListObjectsV2Output" => Some(LIST_OBJECTS_V2_OUTPUT_ORDER),
        _ => None,
    }
}

const LIST_OBJECTS_OUTPUT_ORDER: &[&str] = &[
    "Name",
    "Prefix",
    "Marker",
    "MaxKeys",
    "IsTruncated",
    "Contents",
    "CommonPrefixes",
    "Delimiter",
    "NextMarker",
    "EncodingType",
    "RequestCharged",
];

const LIST_OBJECTS_V2_OUTPUT_ORDER: &[&str] = &[
    "Name",
    "Prefix",
    "MaxKeys",
    "KeyCount",
    "ContinuationToken",
    "IsTruncated",
    "NextContinuationToken",
    "Contents",
    "CommonPrefixes",
    "Delimiter",
    "EncodingType",
    "StartAfter",
    "RequestCharged",
];
