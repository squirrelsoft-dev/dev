//! Test that proto files are correctly compiled

use buildkit_client::BuildKitClient;

#[tokio::test]
async fn test_proto_types_exist() {
    // This test just verifies that the proto types are available
    // We don't need a running BuildKit daemon for this
    
    // If this compiles, it means the proto files were correctly processed
    let _: Option<BuildKitClient> = None;
}
