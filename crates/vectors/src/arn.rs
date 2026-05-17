use marila_aws_compat::AwsError;

/// Format an S3 Vectors bucket ARN to match the shape AWS emits.
///
/// Captured 2026-05-17 (CLAUDE.md C-2):
/// `arn:aws:s3vectors:eu-west-1:625644349722:bucket/<bucket-name>`
pub fn vector_bucket_arn(region: &str, account_id: &str, bucket: &str) -> String {
    format!("arn:aws:s3vectors:{region}:{account_id}:bucket/{bucket}")
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
    resource.strip_prefix("bucket/").ok_or_else(|| {
        AwsError::Validation(format!(
            "vectorBucketArn resource must be `bucket/<name>`, got `{resource}`"
        ))
    })
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
        for bad in ["nope", "arn:aws:s3:::bucket/foo", "arn:aws:s3vectors:::bucket/"] {
            assert!(parse_bucket_name_from_arn(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn parse_accepts_aws_partitions() {
        // The aws-cn / aws-us-gov partitions use a different prefix; we
        // only assert s3vectors here.
        let arn = "arn:aws-cn:s3vectors:cn-north-1:000:bucket/x";
        assert_eq!(parse_bucket_name_from_arn(arn).unwrap(), "x");
    }
}
