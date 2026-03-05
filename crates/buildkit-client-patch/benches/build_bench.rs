//! Benchmarks for buildkit-client
//!
//! Run with: cargo bench

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use buildkit_client::{BuildConfig, Platform, DockerfileSource};
use std::collections::HashMap;
use std::path::PathBuf;

fn bench_platform_parse(c: &mut Criterion) {
    c.bench_function("platform_parse_simple", |b| {
        b.iter(|| {
            Platform::parse(black_box("linux/amd64"))
        })
    });

    c.bench_function("platform_parse_with_variant", |b| {
        b.iter(|| {
            Platform::parse(black_box("linux/arm64/v8"))
        })
    });
}

fn bench_platform_to_string(c: &mut Criterion) {
    let platform = Platform::linux_amd64();

    c.bench_function("platform_to_string", |b| {
        b.iter(|| {
            black_box(&platform).to_string()
        })
    });
}

fn bench_build_config_creation(c: &mut Criterion) {
    c.bench_function("build_config_local", |b| {
        b.iter(|| {
            BuildConfig::local(black_box("./test"))
        })
    });

    c.bench_function("build_config_github", |b| {
        b.iter(|| {
            BuildConfig::github(black_box("https://github.com/user/repo.git"))
        })
    });
}

fn bench_build_config_builder_pattern(c: &mut Criterion) {
    c.bench_function("build_config_full_chain", |b| {
        b.iter(|| {
            BuildConfig::local(black_box("./app"))
                .dockerfile("Dockerfile")
                .tag("myapp:v1")
                .tag("myapp:latest")
                .build_arg("VERSION", "1.0.0")
                .build_arg("ENV", "production")
                .target("production")
                .platform(Platform::linux_arm64())
                .no_cache(true)
                .pull(true)
        })
    });
}

fn bench_session_metadata(c: &mut Criterion) {
    use buildkit_client::session::Session;

    c.bench_function("session_creation", |b| {
        b.iter(|| {
            Session::new()
        })
    });

    c.bench_function("session_metadata_generation", |b| {
        let session = Session::new();
        b.iter(|| {
            session.metadata()
        })
    });
}

fn bench_dockerfile_source_match(c: &mut Criterion) {
    let local_source = DockerfileSource::Local {
        context_path: PathBuf::from("./test"),
        dockerfile_path: None,
    };

    let github_source = DockerfileSource::GitHub {
        repo_url: "https://github.com/user/repo.git".to_string(),
        git_ref: Some("main".to_string()),
        dockerfile_path: None,
        token: None,
    };

    c.bench_function("dockerfile_source_match_local", |b| {
        b.iter(|| {
            match black_box(&local_source) {
                DockerfileSource::Local { .. } => true,
                _ => false,
            }
        })
    });

    c.bench_function("dockerfile_source_match_github", |b| {
        b.iter(|| {
            match black_box(&github_source) {
                DockerfileSource::GitHub { .. } => true,
                _ => false,
            }
        })
    });
}

fn bench_hashmap_operations(c: &mut Criterion) {
    c.bench_function("build_args_insertion", |b| {
        b.iter(|| {
            let mut map = HashMap::new();
            map.insert("VERSION".to_string(), "1.0.0".to_string());
            map.insert("BUILD_DATE".to_string(), "2024-01-01".to_string());
            map.insert("ENV".to_string(), "production".to_string());
            black_box(map)
        })
    });
}

criterion_group!(
    benches,
    bench_platform_parse,
    bench_platform_to_string,
    bench_build_config_creation,
    bench_build_config_builder_pattern,
    bench_session_metadata,
    bench_dockerfile_source_match,
    bench_hashmap_operations,
);

criterion_main!(benches);
