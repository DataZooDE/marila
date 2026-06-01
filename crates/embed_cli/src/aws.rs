//! AWS SDK construction helpers — mirrors the integration-test harness'
//! recipe (`crates/integration_tests/src/harness.rs::aws_sdk_config`) so
//! marila-embed and the contract tests speak to marila exactly the same
//! way.

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3vectors::Client as VectorsClient;

use crate::cli::CommonArgs;

/// Build an `aws-sdk-s3vectors` client pointed at the caller's
/// `--endpoint-url`. For real AWS, omit the local endpoint override —
/// the SDK will pick up creds from the standard provider chain.
pub async fn vectors_client(common: &CommonArgs) -> VectorsClient {
    let endpoint = common.endpoint_url.trim_end_matches('/').to_owned();
    let region = Region::new(common.region.clone());

    let mut builder = aws_config::defaults(BehaviorVersion::latest()).region(region);

    if endpoint_is_local(&endpoint) {
        // Static dummy creds — marila parses but does not verify SigV4.
        // Same recipe as the integration-test harness so the wire shape
        // matches exactly.
        let creds = Credentials::new("marila", "marilasecret", None, None, "marila-embed");
        builder = builder.credentials_provider(creds).endpoint_url(endpoint);
    } else if !endpoint.contains(".amazonaws.com") {
        // Custom endpoint that isn't localhost — still override, but use
        // the standard credential chain.
        builder = builder.endpoint_url(endpoint);
    }
    // Else: real AWS s3vectors, no endpoint override.

    let config = builder.load().await;
    VectorsClient::new(&config)
}

fn endpoint_is_local(endpoint: &str) -> bool {
    endpoint.contains("localhost") || endpoint.contains("127.0.0.1")
}
