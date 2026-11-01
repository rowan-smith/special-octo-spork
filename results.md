# Vyrtel benchmarks

- Generated: 2026-11-01 20:42:32 UTC
- Vyrtel: 0.1.1
- BenchmarkDotNet job: short
- Dataset: 200,000 seeded log events
- Machine: Ubuntu 26.04.1 LTS (X64), 4 logical cores
- Runtime: .NET 10.0.14
- Executable: 12.9 MB (release)
- Data directory: 22.0 MB on disk after seeding
- Storage: 203,200 events in 3 segments · 113.31 bytes/event · 4.08x compression · 7.5% index overhead

## Ingest

| Benchmark | Mean | Median | Min | Max | StdDev | Throughput |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Ingest native JSON array | 3.94 ms | 3.99 ms | 3.63 ms | 4.21 ms | 0.291 ms | 126,793 events/s |
| Ingest native NDJSON batch | 3.57 ms | 3.53 ms | 3.50 ms | 3.68 ms | 0.100 ms | 140,103 events/s |
| Ingest OTLP metrics (500 points) | 2.27 ms | 2.25 ms | 2.19 ms | 2.36 ms | 0.088 ms | 220,513 points/s |
| Ingest OTLP trace (3 spans) | 0.251 ms | 0.233 ms | 0.230 ms | 0.291 ms | 0.034 ms | 11,947 spans/s |

## Query

| Benchmark | Mean | Median | Min | Max | StdDev | Throughput |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Count: level = Error | 7.05 ms | 7.05 ms | 7.04 ms | 7.06 ms | 0.010 ms | 142 ops/s |
| Facets: sampled field values | 14.14 ms | 13.93 ms | 13.85 ms | 14.65 ms | 0.440 ms | 71 ops/s |
| Filter: level = Error | 9.20 ms | 9.13 ms | 9.10 ms | 9.38 ms | 0.155 ms | 109 ops/s |
| Filter: service = payments | 3.75 ms | 3.74 ms | 3.74 ms | 3.77 ms | 0.020 ms | 267 ops/s |
| Full-text: message contains error (scan) | 32.98 ms | 32.39 ms | 32.33 ms | 34.22 ms | 1.08 ms | 30 ops/s |
| Histogram: all logs (60 buckets) | 7.32 ms | 7.32 ms | 7.31 ms | 7.32 ms | 0.002 ms | 137 ops/s |
| Latest logs (limit 200) | 3.20 ms | 3.20 ms | 3.18 ms | 3.21 ms | 0.015 ms | 313 ops/s |
| List metric names | 0.161 ms | 0.161 ms | 0.161 ms | 0.161 ms | 0.000 ms | 6,214 ops/s |
| Lookup: traceId = ... (bloom filter) | 1.83 ms | 1.83 ms | 1.82 ms | 1.84 ms | 0.006 ms | 547 ops/s |
| Metric series (avg over range) | 0.257 ms | 0.255 ms | 0.254 ms | 0.262 ms | 0.004 ms | 3,891 ops/s |
| Range: durationMs > 1990 (block skip) | 109.11 ms | 109.13 ms | 108.54 ms | 109.65 ms | 0.556 ms | 9 ops/s |
| Trace by id (all spans) | 0.255 ms | 0.256 ms | 0.252 ms | 0.258 ms | 0.003 ms | 3,921 ops/s |
| Trace search (limit 50) | 4.58 ms | 4.56 ms | 4.55 ms | 4.62 ms | 0.040 ms | 219 ops/s |

