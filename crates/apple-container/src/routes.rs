/// XPC service name for the Apple Container API server.
pub const SERVICE_NAME: &str = "com.apple.container.apiserver";

/// XPC service name for the Apple Container image service.
pub const IMAGE_SERVICE_NAME: &str = "com.apple.container.core.container-core-images";

/// XPC dictionary key used to identify the route (method) for each request.
pub const ROUTE_KEY: &str = "com.apple.container.xpc.route";

/// XPC dictionary key used for error messages in replies.
pub const ERROR_KEY: &str = "com.apple.container.xpc.error";

/// Routes supported by the Apple Container XPC API.
#[derive(Debug, Clone, Copy)]
pub enum XpcRoute {
    Ping,
    ContainerCreate,
    ContainerList,
    ContainerGet,
    ContainerBootstrap,
    GetDefaultKernel,
    ContainerCreateProcess,
    ContainerStartProcess,
    ContainerWait,
    ContainerResize,
    ContainerKill,
    ContainerStop,
    ContainerDelete,
    ContainerLogs,
    ContainerStats,
    ContainerDiskUsage,
    ContainerExport,
    ContainerDial,
}

impl XpcRoute {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::ContainerCreate => "containerCreate",
            Self::ContainerList => "containerList",
            Self::ContainerGet => "containerGet",
            Self::ContainerBootstrap => "containerBootstrap",
            Self::GetDefaultKernel => "getDefaultKernel",
            Self::ContainerCreateProcess => "containerCreateProcess",
            Self::ContainerStartProcess => "containerStartProcess",
            Self::ContainerWait => "containerWait",
            Self::ContainerResize => "containerResize",
            Self::ContainerKill => "containerKill",
            Self::ContainerStop => "containerStop",
            Self::ContainerDelete => "containerDelete",
            Self::ContainerLogs => "containerLogs",
            Self::ContainerStats => "containerStats",
            Self::ContainerDiskUsage => "containerDiskUsage",
            Self::ContainerExport => "containerExport",
            Self::ContainerDial => "containerDial",
        }
    }
}

/// Well-known XPC dictionary keys used in requests and replies.
pub struct XpcKey;

impl XpcKey {
    pub const ID: &str = "id";
    pub const CONTAINERS: &str = "containers";
    pub const CONTAINER_CONFIG: &str = "containerConfig";
    pub const CONTAINER_OPTIONS: &str = "containerOptions";
    pub const PROCESS_IDENTIFIER: &str = "processIdentifier";
    pub const PROCESS_CONFIG: &str = "processConfig";
    pub const SIGNAL: &str = "signal";
    pub const EXIT_CODE: &str = "exitCode";
    /// Terminal columns, used by `containerResize`.
    pub const WIDTH: &str = "width";
    /// Terminal rows, used by `containerResize`.
    pub const HEIGHT: &str = "height";
    pub const SNAPSHOT: &str = "snapshot";
    pub const STATUS: &str = "status";
    pub const STDIN: &str = "stdin";
    pub const STDOUT: &str = "stdout";
    pub const STDERR: &str = "stderr";
    pub const LOGS: &str = "logs";
    pub const FD: &str = "fd";
    pub const PORT: &str = "port";
    pub const KERNEL: &str = "kernel";
    pub const SYSTEM_PLATFORM: &str = "systemPlatform";
    pub const INIT_IMAGE: &str = "initImage";
    pub const STATISTICS: &str = "statistics";
    pub const ARCHIVE: &str = "archive";
    pub const FORCE: &str = "force";
    pub const STOP_OPTIONS: &str = "stopOptions";

    // Image service keys
    pub const IMAGE_REFERENCE: &str = "imageReference";
    pub const IMAGE_DESCRIPTION: &str = "imageDescription";
    pub const OCI_PLATFORM: &str = "ociPlatform";
    pub const INSECURE_FLAG: &str = "insecureFlag";
    pub const MAX_CONCURRENT_DOWNLOADS: &str = "maxConcurrentDownloads";
    pub const FILESYSTEM: &str = "filesystem";

    /// Host path of the OCI archive passed to `imageLoad`.
    pub const FILE_PATH: &str = "filePath";
    /// Whether `imageLoad` accepts an archive containing rejected members.
    pub const FORCE_LOAD: &str = "forceLoad";
    /// JSON `[ImageDescription]` returned by `imageLoad`.
    pub const IMAGE_DESCRIPTIONS: &str = "imageDescriptions";
    /// JSON `[String]` of archive members `imageLoad` refused.
    pub const REJECTED_MEMBERS: &str = "rejectedMembers";
    /// Target reference for `imageTag`.
    pub const IMAGE_NEW_REFERENCE: &str = "imageNewReference";
}

/// Routes supported by the Apple Container Image Service XPC API.
pub enum ImageRoute {
    ImagePull,
    ImageList,
    ImageUnpack,
    ImageLoad,
    ImageTag,
    SnapshotGet,
}

impl ImageRoute {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ImagePull => "imagePull",
            Self::ImageList => "imageList",
            Self::ImageUnpack => "imageUnpack",
            Self::ImageLoad => "imageLoad",
            Self::ImageTag => "imageTag",
            Self::SnapshotGet => "snapshotGet",
        }
    }
}
