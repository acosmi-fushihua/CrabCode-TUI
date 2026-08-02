use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use acosmi_memory_se::segment_store::{CollectionConfig, SearchEngine};
use acosmi_memory_se::{Distance, HnswConfig, Payload, VectorStorageType};
use acosmi_memory_session::{CollectionSchema, DistanceMetric};
use serde_json::json;
use tempfile::TempDir;

#[derive(Clone, Copy, Debug)]
struct CorpusProfile {
    name: &'static str,
    points: usize,
    dimension: usize,
    queries: usize,
    changed_points: usize,
    added_points: usize,
}

#[derive(Clone, Copy, Debug)]
struct PerfThresholds {
    upsert_p95_ms: f64,
    incremental_p95_ms: f64,
    optimize_ms: f64,
    search_p95_ms: f64,
    index_size_mib: f64,
    rss_delta_mib: f64,
}

#[derive(Debug)]
struct OperationStats {
    count: usize,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Debug)]
struct BenchSummary {
    profile: CorpusProfile,
    initial_upsert: OperationStats,
    incremental_reindex: OperationStats,
    optimize_collection: OperationStats,
    search: OperationStats,
    final_points: usize,
    index_size_mib: f64,
    rss_start_mib: Option<f64>,
    rss_peak_mib: Option<f64>,
    total_wall_ms: f64,
}

const COLLECTION: &str = "phase3_vector_perf";

fn vector_schema(dimension: usize) -> CollectionSchema {
    CollectionSchema {
        vector_dim: dimension,
        distance: DistanceMetric::Cosine,
        fields: Vec::new(),
    }
}

fn vector_config(schema: &CollectionSchema) -> CollectionConfig {
    CollectionConfig {
        dimension: schema.vector_dim,
        distance: match schema.distance {
            DistanceMetric::Cosine => Distance::Cosine,
            DistanceMetric::Euclid => Distance::Euclid,
            DistanceMetric::DotProduct => Distance::Dot,
        },
        sparse_vectors: false,
        hnsw: Some(HnswConfig {
            m: 16,
            ef_construct: 100,
            full_scan_threshold: 0,
            max_indexing_threads: 0,
            on_disk: None,
            payload_m: None,
            inline_storage: None,
        }),
        quantization: None,
        storage_type: VectorStorageType::InRamChunkedMmap,
        datatype: None,
    }
}

fn point_id(index: usize) -> String {
    format!("00000000-0000-0000-0004-{index:012x}")
}

fn payload(index: usize, generation: usize) -> Payload {
    Payload::from(
        json!({
            "idx": index as u64,
            "generation": generation as u64,
            "bucket": format!("bucket-{}", index % 16),
            "kind": "phase3_vector_perf"
        })
        .as_object()
        .expect("benchmark payload is a JSON object")
        .clone(),
    )
}

fn deterministic_unit_vector(index: usize, dimension: usize, salt: usize) -> Vec<f32> {
    let anchor = (index.wrapping_mul(31).wrapping_add(salt.wrapping_mul(17))) % dimension;
    let mut vector = Vec::with_capacity(dimension);

    for dim in 0..dimension {
        let mut value = deterministic_noise(index, dim, salt) * 0.04;
        if dim == anchor {
            value += 1.0;
        }
        vector.push(value);
    }

    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    for value in &mut vector {
        *value /= norm;
    }

    vector
}

fn deterministic_noise(index: usize, dim: usize, salt: usize) -> f32 {
    let mut x = (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= (dim as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= (salt as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    ((x >> 40) as f32 / ((1_u64 << 24) as f32)) - 0.5
}

fn run_profile(profile: CorpusProfile) -> BenchSummary {
    let temp_dir = TempDir::new().expect("create benchmark temp dir");
    let engine = SearchEngine::new(temp_dir.path()).expect("create search engine");
    let total_started = Instant::now();
    let rss_start_mib = current_rss_mib();
    let mut rss_peak_mib = rss_start_mib;

    let schema = vector_schema(profile.dimension);
    engine
        .create_collection(COLLECTION, &vector_config(&schema))
        .expect("create benchmark collection");
    assert_eq!(
        engine
            .collection_config(COLLECTION)
            .expect("benchmark collection config should exist")
            .dimension,
        schema.vector_dim,
        "benchmark collection dimension must come from CollectionSchema.vector_dim"
    );

    let mut upsert_durations = Vec::with_capacity(profile.points);
    for index in 0..profile.points {
        let vector = deterministic_unit_vector(index, profile.dimension, 0);
        let point_id = point_id(index);
        let payload = payload(index, 0);
        upsert_durations.push(time_it(|| {
            engine
                .upsert(COLLECTION, &point_id, &vector, Some(&payload))
                .expect("initial vector upsert");
        }));
    }
    rss_peak_mib = max_optional(rss_peak_mib, current_rss_mib());

    let incremental_total = profile.changed_points + profile.added_points;
    let mut incremental_durations = Vec::with_capacity(incremental_total);
    for offset in 0..profile.changed_points {
        let index = offset % profile.points;
        let vector = deterministic_unit_vector(index, profile.dimension, 1);
        let point_id = point_id(index);
        let payload = payload(index, 1);
        incremental_durations.push(time_it(|| {
            engine
                .upsert(COLLECTION, &point_id, &vector, Some(&payload))
                .expect("incremental changed vector upsert");
        }));
    }
    for offset in 0..profile.added_points {
        let index = profile.points + offset;
        let vector = deterministic_unit_vector(index, profile.dimension, 2);
        let point_id = point_id(index);
        let payload = payload(index, 1);
        incremental_durations.push(time_it(|| {
            engine
                .upsert(COLLECTION, &point_id, &vector, Some(&payload))
                .expect("incremental added vector upsert");
        }));
    }
    rss_peak_mib = max_optional(rss_peak_mib, current_rss_mib());

    let stopped = AtomicBool::new(false);
    let optimize_duration = time_it(|| {
        assert!(
            engine
                .optimize_collection(COLLECTION, &stopped)
                .expect("optimize vector collection"),
            "HNSW-backed benchmark collection should optimize"
        );
    });
    rss_peak_mib = max_optional(rss_peak_mib, current_rss_mib());

    engine
        .flush(COLLECTION)
        .expect("flush benchmark collection");

    let mut search_durations = Vec::with_capacity(profile.queries);
    for query_index in 0..profile.queries {
        let vector =
            deterministic_unit_vector(query_index % profile.final_points(), profile.dimension, 1);
        search_durations.push(time_it(|| {
            let hits = engine
                .search(COLLECTION, &vector, None, 10, None)
                .expect("vector search");
            assert!(!hits.is_empty(), "benchmark search should return hits");
            assert!(hits.iter().all(|hit| hit.score.is_finite()));
        }));
    }
    rss_peak_mib = max_optional(rss_peak_mib, current_rss_mib());

    BenchSummary {
        profile,
        initial_upsert: OperationStats::from_durations(&upsert_durations),
        incremental_reindex: OperationStats::from_durations(&incremental_durations),
        optimize_collection: OperationStats::from_durations(&[optimize_duration]),
        search: OperationStats::from_durations(&search_durations),
        final_points: profile.final_points(),
        index_size_mib: bytes_to_mib(dir_size_bytes(temp_dir.path())),
        rss_start_mib,
        rss_peak_mib,
        total_wall_ms: duration_ms(total_started.elapsed()),
    }
}

impl CorpusProfile {
    fn final_points(self) -> usize {
        self.points + self.added_points
    }
}

impl OperationStats {
    fn from_durations(durations: &[Duration]) -> Self {
        let mut values = durations
            .iter()
            .map(|duration| duration_ms(*duration))
            .collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);

        Self {
            count: values.len(),
            p50_ms: percentile(&values, 0.50),
            p95_ms: percentile(&values, 0.95),
            max_ms: values.last().copied().unwrap_or(0.0),
        }
    }
}

impl BenchSummary {
    fn rss_delta_mib(&self) -> Option<f64> {
        Some(self.rss_peak_mib? - self.rss_start_mib?)
    }

    fn print_summary(&self) {
        println!(
            "profile={name} points={points}->{final_points} dim={dim} queries={queries} \
             total_wall_ms={total:.2} index_size_mib={index:.2} rss_start_mib={rss_start} \
             rss_peak_mib={rss_peak} rss_delta_mib={rss_delta}",
            name = self.profile.name,
            points = self.profile.points,
            final_points = self.final_points,
            dim = self.profile.dimension,
            queries = self.profile.queries,
            total = self.total_wall_ms,
            index = self.index_size_mib,
            rss_start = fmt_optional(self.rss_start_mib),
            rss_peak = fmt_optional(self.rss_peak_mib),
            rss_delta = fmt_optional(self.rss_delta_mib()),
        );
        print_operation("upsert", &self.initial_upsert);
        print_operation("incremental_reindex", &self.incremental_reindex);
        print_operation("optimize_collection", &self.optimize_collection);
        print_operation("search", &self.search);
    }
}

fn smoke_profile() -> CorpusProfile {
    CorpusProfile {
        name: "smoke",
        points: 128,
        dimension: 32,
        queries: 16,
        changed_points: 16,
        added_points: 8,
    }
}

fn manual_profiles(selector: &str) -> Vec<CorpusProfile> {
    let small = CorpusProfile {
        name: "small",
        points: 512,
        dimension: 64,
        queries: 32,
        changed_points: 64,
        added_points: 32,
    };
    let medium = CorpusProfile {
        name: "medium",
        points: 2_500,
        dimension: 128,
        queries: 96,
        changed_points: 250,
        added_points: 125,
    };
    let large = CorpusProfile {
        name: "large",
        points: 10_000,
        dimension: 256,
        queries: 160,
        changed_points: 1_000,
        added_points: 500,
    };

    match selector {
        "small" => vec![small],
        "medium" => vec![medium],
        "large" => vec![large],
        "all" => vec![small, medium, large],
        other => {
            panic!("unsupported ACOSMI_MEMORY_SE_BENCH_PROFILE={other}; use small|medium|large|all")
        }
    }
}

fn smoke_thresholds() -> PerfThresholds {
    PerfThresholds {
        upsert_p95_ms: 250.0,
        incremental_p95_ms: 250.0,
        optimize_ms: 10_000.0,
        search_p95_ms: 500.0,
        index_size_mib: 256.0,
        rss_delta_mib: 512.0,
    }
}

fn manual_thresholds(profile: &str) -> PerfThresholds {
    match profile {
        "small" => PerfThresholds {
            upsert_p95_ms: 50.0,
            incremental_p95_ms: 50.0,
            optimize_ms: 20_000.0,
            search_p95_ms: 50.0,
            index_size_mib: 512.0,
            rss_delta_mib: 1_024.0,
        },
        "medium" => PerfThresholds {
            upsert_p95_ms: 100.0,
            incremental_p95_ms: 100.0,
            optimize_ms: 60_000.0,
            search_p95_ms: 100.0,
            index_size_mib: 1_024.0,
            rss_delta_mib: 2_048.0,
        },
        "large" => PerfThresholds {
            upsert_p95_ms: 150.0,
            incremental_p95_ms: 150.0,
            optimize_ms: 180_000.0,
            search_p95_ms: 250.0,
            index_size_mib: 4_096.0,
            rss_delta_mib: 4_096.0,
        },
        other => panic!("missing manual thresholds for profile {other}"),
    }
}

fn assert_within_thresholds(summary: &BenchSummary, thresholds: PerfThresholds) {
    assert!(
        summary.initial_upsert.p95_ms <= thresholds.upsert_p95_ms,
        "upsert p95 {:.2}ms exceeded threshold {:.2}ms",
        summary.initial_upsert.p95_ms,
        thresholds.upsert_p95_ms
    );
    assert!(
        summary.incremental_reindex.p95_ms <= thresholds.incremental_p95_ms,
        "incremental reindex p95 {:.2}ms exceeded threshold {:.2}ms",
        summary.incremental_reindex.p95_ms,
        thresholds.incremental_p95_ms
    );
    assert!(
        summary.optimize_collection.p95_ms <= thresholds.optimize_ms,
        "optimize_collection {:.2}ms exceeded threshold {:.2}ms",
        summary.optimize_collection.p95_ms,
        thresholds.optimize_ms
    );
    assert!(
        summary.search.p95_ms <= thresholds.search_p95_ms,
        "search p95 {:.2}ms exceeded threshold {:.2}ms",
        summary.search.p95_ms,
        thresholds.search_p95_ms
    );
    assert!(
        summary.index_size_mib <= thresholds.index_size_mib,
        "index size {:.2}MiB exceeded threshold {:.2}MiB",
        summary.index_size_mib,
        thresholds.index_size_mib
    );
    if let Some(delta_mib) = summary.rss_delta_mib() {
        assert!(
            delta_mib <= thresholds.rss_delta_mib,
            "RSS delta {:.2}MiB exceeded threshold {:.2}MiB",
            delta_mib,
            thresholds.rss_delta_mib
        );
    }
}

fn time_it<T>(f: impl FnOnce() -> T) -> Duration {
    let started = Instant::now();
    f();
    started.elapsed()
}

fn percentile(sorted_values: &[f64], percentile: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }
    let index = ((sorted_values.len() - 1) as f64 * percentile).ceil() as usize;
    sorted_values[index]
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn dir_size_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                return 0;
            };
            if metadata.is_dir() {
                dir_size_bytes(&path)
            } else {
                metadata.len()
            }
        })
        .sum()
}

fn current_rss_mib() -> Option<f64> {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let rss_kib = stdout.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(rss_kib / 1024.0)
}

fn max_optional(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn fmt_optional(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.2}"))
}

fn print_operation(name: &str, stats: &OperationStats) {
    println!(
        "  {name}: count={count} p50_ms={p50:.2} p95_ms={p95:.2} max_ms={max:.2}",
        count = stats.count,
        p50 = stats.p50_ms,
        p95 = stats.p95_ms,
        max = stats.max_ms,
    );
}

#[test]
fn phase3_vector_perf_smoke_gate() {
    let summary = run_profile(smoke_profile());
    summary.print_summary();
    assert_within_thresholds(&summary, smoke_thresholds());
}

#[test]
#[ignore = "manual P4 benchmark: run with --ignored --nocapture, preferably --release"]
fn phase3_vector_perf_manual_benchmark() {
    let selector =
        std::env::var("ACOSMI_MEMORY_SE_BENCH_PROFILE").unwrap_or_else(|_| "all".to_owned());
    let enforce_thresholds = std::env::var("ACOSMI_MEMORY_SE_BENCH_ENFORCE")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));

    println!("ACOSMI_MEMORY_SE_PHASE3_VECTOR_BENCH_SUMMARY");
    println!(
        "profile_selector={selector} enforce_thresholds={enforce_thresholds} \
         note=incremental_reindex_is_changed_plus_added_vector_upsert_before_hnsw_optimize"
    );

    for profile in manual_profiles(&selector) {
        let summary = run_profile(profile);
        summary.print_summary();
        if enforce_thresholds {
            assert_within_thresholds(&summary, manual_thresholds(profile.name));
        }
    }
}
