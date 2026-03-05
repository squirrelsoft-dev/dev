//! Generated protobuf code for BuildKit gRPC API

// Include the generated proto code
pub mod moby {
    pub mod buildkit {
        pub mod v1 {
            tonic::include_proto!("moby.buildkit.v1");

            pub mod types {
                tonic::include_proto!("moby.buildkit.v1.types");
            }

            pub mod sourcepolicy {
                tonic::include_proto!("moby.buildkit.v1.sourcepolicy");
            }
        }
    }

    pub mod filesync {
        pub mod v1 {
            tonic::include_proto!("moby.filesync.v1");
        }
    }

    pub mod secrets {
        pub mod v1 {
            tonic::include_proto!("moby.buildkit.secrets.v1");
        }
    }
}

pub mod pb {
    tonic::include_proto!("pb");
}

pub mod fsutil {
    pub mod types {
        tonic::include_proto!("fsutil.types");
    }
}

pub mod google {
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

// Re-export commonly used types
pub use moby::buildkit::v1::*;
