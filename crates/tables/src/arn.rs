use marila_aws_compat::AwsError;

/// Format an S3 Tables bucket ARN to match what AWS emits (CLAUDE.md C-9):
/// `arn:aws:s3tables:<region>:<account>:bucket/<name>`
pub fn table_bucket_arn(region: &str, account_id: &str, name: &str) -> String {
    format!("arn:aws:s3tables:{region}:{account_id}:bucket/{name}")
}

/// Extract the bucket-name suffix from a table-bucket ARN. Used by
/// GetTableBucket / DeleteTableBucket which take the ARN url-encoded in
/// the path.
pub fn parse_bucket_name_from_arn(arn: &str) -> Result<&str, AwsError> {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" || parts[2] != "s3tables" {
        return Err(AwsError::Validation(format!(
            "tableBucketARN does not look like an S3 Tables bucket ARN: {arn}"
        )));
    }
    let resource = parts[5];
    let name = resource.strip_prefix("bucket/").ok_or_else(|| {
        AwsError::Validation(format!(
            "tableBucketARN resource must be `bucket/<name>`, got `{resource}`"
        ))
    })?;
    if name.is_empty() {
        return Err(AwsError::Validation(format!(
            "tableBucketARN has an empty bucket name: {arn}"
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
            table_bucket_arn("eu-west-1", "625644349722", "demo"),
            "arn:aws:s3tables:eu-west-1:625644349722:bucket/demo"
        );
    }

    #[test]
    fn parse_round_trips() {
        let arn = table_bucket_arn("eu-west-1", "625644349722", "my-bucket");
        assert_eq!(parse_bucket_name_from_arn(&arn).unwrap(), "my-bucket");
    }

    #[test]
    fn parse_rejects_wrong_service() {
        assert!(parse_bucket_name_from_arn("arn:aws:s3vectors:eu-west-1:0:bucket/x").is_err());
        assert!(parse_bucket_name_from_arn("nope").is_err());
        assert!(parse_bucket_name_from_arn("arn:aws:s3tables:::bucket/").is_err());
    }
}
