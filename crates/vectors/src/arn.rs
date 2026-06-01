use marila_aws_compat::AwsError;

/// Format an S3 Vectors bucket ARN to match the shape AWS emits.
///
/// Captured 2026-05-17 (doc/GAP_ANALYSIS.md):
/// `arn:aws:s3vectors:eu-west-1:625644349722:bucket/<bucket-name>`
pub fn vector_bucket_arn(region: &str, account_id: &str, bucket: &str) -> String {
    format!("arn:aws:s3vectors:{region}:{account_id}:bucket/{bucket}")
}

/// Format an S3 Vectors index ARN.
///
/// AWS uses a nested resource shape (doc/GAP_ANALYSIS.md):
/// `arn:aws:s3vectors:<region>:<account>:bucket/<b>/index/<i>`
pub fn vector_index_arn(region: &str, account_id: &str, bucket: &str, index: &str) -> String {
    format!("arn:aws:s3vectors:{region}:{account_id}:bucket/{bucket}/index/{index}")
}

/// Extract `(bucket, index)` from an S3 Vectors index ARN.
pub fn parse_index_from_arn(arn: &str) -> Result<(&str, &str), AwsError> {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" || parts[2] != "s3vectors" {
        return Err(AwsError::Validation(format!(
            "indexArn does not look like an S3 Vectors index ARN: {arn}"
        )));
    }
    let resource = parts[5];
    let rest = resource.strip_prefix("bucket/").ok_or_else(|| {
        AwsError::Validation(format!(
            "indexArn resource must start with `bucket/<bucket>/index/<index>`, got `{resource}`"
        ))
    })?;
    let (bucket, tail) = rest.split_once('/').ok_or_else(|| {
        AwsError::Validation(format!("indexArn missing `/index/<name>` suffix: {arn}"))
    })?;
    let index = tail.strip_prefix("index/").ok_or_else(|| {
        AwsError::Validation(format!(
            "indexArn second segment must be `index/<name>`, got `{tail}`"
        ))
    })?;
    if bucket.is_empty() || index.is_empty() {
        return Err(AwsError::Validation(format!(
            "indexArn has an empty bucket or index name: {arn}"
        )));
    }
    Ok((bucket, index))
}

/// Extract the bucket-name suffix from an S3 Vectors bucket ARN.
///
/// Returns a `ValidationException`-shaped error for inputs that don't
/// look like an s3vectors bucket ARN. We don't try to validate the
/// region or account against the running configuration — that would
/// reject legitimate ARNs written by clients pointing at a differently
/// configured marila, which isn't a contract AWS exposes either.
pub fn parse_bucket_name_from_arn(arn: &str) -> Result<&str, AwsError> {
    // Required structure: arn:aws:s3vectors:<region>:<account>:bucket/<name>
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" || parts[2] != "s3vectors" {
        return Err(AwsError::Validation(format!(
            "vectorBucketArn does not look like an S3 Vectors bucket ARN: {arn}"
        )));
    }
    let resource = parts[5];
    let name = resource.strip_prefix("bucket/").ok_or_else(|| {
        AwsError::Validation(format!(
            "vectorBucketArn resource must be `bucket/<name>`, got `{resource}`"
        ))
    })?;
    if name.is_empty() {
        return Err(AwsError::Validation(format!(
            "vectorBucketArn has an empty bucket name: {arn}"
        )));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_shape_matches_aws() {
        assert_eq!(
            vector_bucket_arn("eu-west-1", "625644349722", "demo"),
            "arn:aws:s3vectors:eu-west-1:625644349722:bucket/demo"
        );
    }

    #[test]
    fn parse_round_trips() {
        let arn = vector_bucket_arn("eu-west-1", "625644349722", "my-bucket");
        assert_eq!(parse_bucket_name_from_arn(&arn).unwrap(), "my-bucket");
    }

    #[test]
    fn parse_rejects_garbage() {
        for bad in [
            "nope",
            "arn:aws:s3:::bucket/foo",
            "arn:aws:s3vectors:::bucket/",
        ] {
            assert!(
                parse_bucket_name_from_arn(bad).is_err(),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn parse_accepts_aws_partitions() {
        // The aws-cn / aws-us-gov partitions use a different prefix; we
        // only assert s3vectors here.
        let arn = "arn:aws-cn:s3vectors:cn-north-1:000:bucket/x";
        assert_eq!(parse_bucket_name_from_arn(arn).unwrap(), "x");
    }

    #[test]
    fn index_arn_round_trips() {
        let arn = vector_index_arn("eu-west-1", "625644349722", "demo", "i1");
        assert_eq!(
            arn,
            "arn:aws:s3vectors:eu-west-1:625644349722:bucket/demo/index/i1"
        );
        assert_eq!(parse_index_from_arn(&arn).unwrap(), ("demo", "i1"));
    }

    #[test]
    fn index_arn_rejects_missing_index_segment() {
        let arn = "arn:aws:s3vectors:eu-west-1:0:bucket/demo";
        assert!(parse_index_from_arn(arn).is_err());
    }
}
