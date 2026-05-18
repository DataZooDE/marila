//! Contract test for marila's `501 NotImplementedException` fallback
//! (REQUIREMENTS.md FV-7).
//!
//! Unlike the round-trip contracts, this file has **no aws_* variants** —
//! the deliberately-not-implemented ops *do* exist on real AWS, so a
//! cross-target comparison would assert different shapes. Here we only
//! verify marila emits the right wire response.
//!
//! We hit the endpoints with raw HTTP rather than going through the SDK
//! because the SDK would refuse to encode the (intentionally empty)
//! input shapes — and because we're explicitly testing the *envelope*,
//! not any operation semantics.

use marila_integration_tests::harness::{LOCAL_ENDPOINT, MarilaProcess};

const UNIMPLEMENTED_OPS: &[&str] = &[
    "PutVectorBucketPolicy",
    "GetVectorBucketPolicy",
    "DeleteVectorBucketPolicy",
    "ListTagsForResource",
    "TagResource",
    "UntagResource",
];

#[tokio::test]
async fn local_unimplemented_ops_return_501_with_envelope() {
    let _marila = MarilaProcess::start();
    let client = reqwest::Client::new();

    for op in UNIMPLEMENTED_OPS {
        let url = format!("{LOCAL_ENDPOINT}/{op}");
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap_or_else(|e| panic!("POST /{op} failed: {e}"));

        assert_eq!(
            resp.status().as_u16(),
            501,
            "/{op} must return HTTP 501, got {}",
            resp.status()
        );

        let error_type = resp
            .headers()
            .get("x-amzn-errortype")
            .unwrap_or_else(|| panic!("/{op} response missing x-amzn-errortype header"))
            .to_str()
            .unwrap();
        assert_eq!(
            error_type, "NotImplementedException",
            "/{op} must mark itself as NotImplementedException"
        );

        let body: serde_json::Value = resp
            .json()
            .await
            .unwrap_or_else(|e| panic!("/{op} response body wasn't JSON: {e}"));
        let message = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("/{op} body missing lowercase `message` field"));
        assert!(
            message.contains(op),
            "/{op} message must mention the op name, got {message:?}"
        );
    }
}
