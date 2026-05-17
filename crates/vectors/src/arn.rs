/// Format an S3 Vectors bucket ARN to match the shape AWS emits.
///
/// Captured 2026-05-17 (CLAUDE.md C-2):
/// `arn:aws:s3vectors:eu-west-1:625644349722:bucket/<bucket-name>`
pub fn vector_bucket_arn(region: &str, account_id: &str, bucket: &str) -> String {
    format!("arn:aws:s3vectors:{region}:{account_id}:bucket/{bucket}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_matches_aws() {
        assert_eq!(
            vector_bucket_arn("eu-west-1", "625644349722", "demo"),
            "arn:aws:s3vectors:eu-west-1:625644349722:bucket/demo"
        );
    }
}
