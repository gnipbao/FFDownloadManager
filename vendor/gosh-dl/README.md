# gosh-dl

A fast, embeddable download engine for Rust applications. Supports HTTP/HTTPS with multi-connection acceleration and BitTorrent features including DHT, PEX, outgoing encryption, and WebSeeds. Feature coverage and limits are listed below.

[![Crates.io](https://img.shields.io/crates/v/gosh-dl.svg)](https://crates.io/crates/gosh-dl)
[![Documentation](https://docs.rs/gosh-dl/badge.svg)](https://docs.rs/gosh-dl)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## Why gosh-dl?

gosh-dl brings download functionality directly into your Rust application as a native library, eliminating the complexity of managing external processes, parsing JSON-RPC responses, or bundling platform-specific binaries. Modern applications demand seamless integration, and gosh-dl delivers exactly that; async function calls that feel natural in your codebase, compile-time type safety that catches errors before runtime, and shared memory that keeps your application lightweight and responsive.

Whether you're building a media application that needs BitTorrent with streaming support, a package manager requiring resilient HTTP downloads with checksums and mirrors, or any software that moves files across the network, gosh-dl provides native download APIs with the feature coverage listed below. Multi-connection acceleration splits large downloads across parallel connections for maximum throughput. Automatic resume with ETag validation ensures interrupted transfers pick up exactly where they left off. BitTorrent support includes DHT for trackerless operation, peer exchange for efficient swarm discovery, and protocol encryption for privacy.

The engine handles the complexity of segmented downloads, tracker communication, DHT peer discovery, and connection encryption while exposing a clean, intuitive API that integrates naturally with Tokio-based applications. Priority queues let you control which downloads matter most, bandwidth scheduling adapts to time-of-day constraints, and persistence — built-in SQLite, JSON sidecar files, or your own `Storage` implementation — supports recovery across restarts when a durable storage implementation is configured.

A standalone CLI is available in the companion `gosh-dl-cli` project for users who want command-line access to the engine.

## Features

### Core Features with Automated Tests

| Feature | Details |
|---------|---------|
| Multi-connection HTTP/HTTPS | Up to 16 parallel connections per download |
| Output filenames | Explicit names, URL fallback, and Content-Disposition when no filename is already selected; see HTTP output paths below |
| Custom headers | User-Agent, Referer, cookies, arbitrary headers |
| Checksum verification | MD5, SHA-256 |
| Concurrent download management | Priority queue (Critical/High/Normal/Low) |
| Pause / resume / cancel | Full lifecycle control, per download or in batch (`pause_all` / `resume_all` / `cancel_all`) |
| Rate limiting | Global + per-download byte-rate limits covering segmented HTTP, single-stream HTTP, torrent peers, and webseeds |
| Streaming read API | `open_reader(id, offset)` yields an `AsyncRead` over an in-progress download; torrent piece selection follows the read head |
| Mirror segment striping | Segments download from multiple mirrors in parallel, with per-URL health tracking and size cross-checks |
| Verify / repair | `verify(id)` re-checks checksums (HTTP) or re-hashes pieces (torrent); `repair(id)` re-downloads what's bad |
| Metalink (RFC 5854) | `add_metalink` expands `.meta4` documents into downloads with mirrors and checksums (feature `metalink`) |
| Cross-restart resume validation | ETag/Last-Modified persisted and checked on resume; changed remote files restart from zero |
| Event system | Broadcast channels for progress, state changes |
| Global statistics | Active count, aggregate speeds |
| SQLite persistence | WAL mode, schema versioning, crash recovery |
| Pluggable persistence | Inject any `Storage` impl via `with_storage()`; built-in `FileStorage` JSON sidecars (aria2 control-file analog) |

### Tested BitTorrent Core

| Feature | BEP | Details |
|---------|-----|---------|
| .torrent parsing | 3 | Single-file and multi-file |
| Magnet URI | 9 | Metadata fetching from peers |
| Multi-peer downloading | 3 | Piece selection, block pipelining |
| Piece hash verification | 3 | SHA-1 per piece |
| HTTP & UDP trackers | 3, 15 | Announce, scrape |
| Sequential download | — | For streaming playback |
| Torrent crash recovery | — | Resume from SQLite-stored torrent data |
| IPv6 tracker peers | 7 | Compact `peers6` parsing |
| Inbound peer connections | 3 | TCP listener on the configured port range; serves peers while downloading and seeding |
| Keep-alive & re-announce | 3 | Idle peers kept alive; periodic tracker re-announce honoring the announce interval |
| Rarest-first piece selection | — | Live availability tracking from bitfields/have messages |

### Implemented, Lightly Tested

| Feature | BEP | Notes |
|---------|-----|-------|
| DHT peer discovery | 5 | Works, disabled in CI tests |
| Peer Exchange (PEX) | 11 | Implemented, disabled in CI tests |
| Local Peer Discovery | 14 | Implemented, disabled in CI tests |
| Message Stream Encryption | MSE/PE | RC4 + DH key exchange, unit tests only |
| WebSeeds | 17, 19 | Hoffman + GetRight, including cross-file pieces |
| uTP transport | 29 | Driver-task architecture with LEDBAT, retransmission, selective ACK; loopback + packet-loss + full-torrent-transfer tests; opt-in |
| Endgame mode | — | Duplicate requests to multiple peers with cancels on receipt; toggle via `enable_endgame` |
| File preallocation | — | `allocation_mode` config, applied before torrent verification |
| HTTP resume | — | ETag/Last-Modified validation |
| Mirror/failover | — | Automatic failover to alternate URLs, plus per-segment mirror striping |
| Bandwidth scheduling | — | Time-of-day rules with live runtime limit updates |
| Recursive HTTP mirroring | — | Feature-gated via `recursive-http`; crawls HTML directory indexes with bounded-concurrency discovery and expands into ordinary HTTP downloads |
| Private torrent handling | 27 | Disables DHT/PEX/LPD |
| Choking algorithm | — | Unchoke rotation, optimistic unchoking |

### Planned / Stub

| Feature | Notes |
|---------|-------|
| DHT IPv6 | Depends on upstream `mainline` crate |
| MSE responder (inbound encryption) | Outgoing MSE works (incl. PadB handling); inbound connections are plaintext-only for now |

Proxy support is wired through reqwest but lacks dedicated interoperability tests.

Release validation uses local protocol fixtures and a Linux/macOS/Windows
feature matrix. It does not establish interoperability with every public
torrent swarm, proxy, or directory-index server. See [ROLLOUT.md](ROLLOUT.md)
for the rollout audit and remaining validation work.

## Quick Start

Requires Rust 1.85 or newer. Add the published crate to your `Cargo.toml`:

```toml
[dependencies]
gosh-dl = "0.6.3"
tokio = { version = "1", features = ["full"] }
```

Optional features: `recursive-http` (directory mirroring) and `metalink`
(RFC 5854 `.meta4` documents). To enable recursive HTTP directory mirroring:

```toml
[dependencies]
gosh-dl = { version = "0.6.3", features = ["recursive-http"] }
tokio = { version = "1", features = ["full"] }
```

Starting with 0.6.2, the release workflow publishes to crates.io before creating
the GitHub release. Version 0.6.1 was distributed on GitHub only.

Basic usage:

```rust
use gosh_dl::{DownloadEngine, EngineConfig, DownloadOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = DownloadEngine::new(EngineConfig::default()).await?;
    // Subscribe before enqueueing so fast downloads cannot finish unseen.
    let mut events = engine.subscribe();

    // HTTP download
    let id = engine.add_http(
        "https://example.com/file.zip",
        DownloadOptions::default(),
    ).await?;

    while let Ok(event) = events.recv().await {
        println!("Event: {:?}", event);
        match event {
            gosh_dl::DownloadEvent::Completed { id: event_id } if event_id == id => break,
            gosh_dl::DownloadEvent::Failed { id: event_id, error, .. } if event_id == id => {
                return Err(error.into());
            }
            _ => {}
        }
    }

    engine.shutdown().await?;
    Ok(())
}
```

## API Overview

All public types are available at the crate root via re-exports. For explicit imports, use `gosh_dl::protocol`:

```rust
use gosh_dl::protocol::{DownloadEvent, DownloadStatus, ProtocolError};
```

### Download Management

```rust
// Add downloads
let http_id = engine.add_http(url, options).await?;
let torrent_id = engine.add_torrent(&torrent_bytes, options).await?;
let magnet_id = engine.add_magnet(magnet_uri, options).await?;

// Control
engine.pause(id).await?;
engine.resume(id).await?;
engine.cancel(id, delete_files).await?;

// Batch control (aria2 pauseAll/unpauseAll analogs).
// Pauses queued downloads too, and returns per-download outcomes.
let result = engine.pause_all().await;
println!("paused {}, skipped {}", result.succeeded.len(), result.skipped.len());
engine.resume_all().await;
engine.cancel_all(delete_files).await;

// Priority
engine.set_priority(id, DownloadPriority::High)?;

// Status
let status = engine.status(id);
let all = engine.list();
let active = engine.active();
let waiting = engine.waiting();
let stopped = engine.stopped();
let stats = engine.global_stats();
```

### Output Paths and Download Lifecycle

Set `DownloadOptions::filename` for a deterministic output name. The engine
otherwise selects the final URL path segment when present; a server's
Content-Disposition filename is used only if no filename has already been
selected. A URL-derived name therefore takes precedence over the header.

Relative names such as `subdir/file.zip` are supported. Empty names, parent
traversal, and absolute paths are rejected before enqueueing. Both transfer
paths create parent directories, and completion preserves the relative path
for subsequent reads, verification, repair, and deletion.

HEAD probes carry the same custom headers, Referer, and cookies as the
download. If the probe fails, the engine tries GET as a single stream and
ignores metadata from the failed HEAD response.

Pausing a torrent stops its worker and releases its concurrency slot. Resume
reconstructs a worker from available metainfo and re-enters the priority
queue, re-checking existing pieces during startup. A queued torrent remains
paused until explicitly resumed. File selection, sequential mode, and limits
are retained.

### Streaming Reads

Read a download's bytes while it is still in flight — for torrents, the read
position feeds back into piece selection so the swarm fetches what the reader
needs next:

```rust
use tokio::io::AsyncReadExt;

let mut reader = engine.open_reader(id, 0)?;
let mut buf = [0u8; 64 * 1024];
loop {
    let n = reader.read(&mut buf).await?;
    if n == 0 { break; } // download fully read (or gone)
    // feed a media player, hash incrementally, ...
}
```

HTTP readers expose only the contiguous downloaded prefix, including after
pause or restart of a segmented download. Torrent readers use verified pieces;
with persisted metainfo they can read restored files without starting peers.
Dropping a reader stops its background pump.

EOF can mean completion or a short stream caused by failure, cancellation,
removal, or a missing/truncated completed file. `total_size()` is the size
known when the reader was opened; for a reader opened at `offset`, compare
the bytes received with `total_size.saturating_sub(offset)` when known.
An unknown size requires another integrity check, such as a checksum.

### Verify & Repair

Pause active transfers before verification. Queued, connecting, and downloading
states are rejected. HTTP verification uses the configured checksum, or only
regular-file presence and size when no checksum is supplied; a size check does
not detect same-size corruption. Torrent verification re-hashes pieces, using
persisted metainfo when no live handle exists.

```rust
let report = engine.verify(id).await?;   // checksum (HTTP) or piece re-hash (torrent)
if !report.valid {
    engine.repair(id).await?;            // re-download what's bad
}
```

`repair()` returns the verification report from before repair and queues work
when data is bad; returning from this call does not mean repair has finished.
HTTP repair restarts from zero. Torrent repair reconstructs the worker and
re-checks existing pieces so missing or corrupt pieces can be fetched again.
Observe download events/status for the resulting transfer.

### Metalink

With the `metalink` feature enabled, RFC 5854 `.meta4` documents expand into
downloads wired with their mirrors and checksums:

```rust
let ids = engine.add_metalink_file("release.meta4".as_ref(), DownloadOptions::default()).await?;
```

Supported URLs are HTTP/HTTPS, ordered by priority. Other transports are
skipped, and files with no supported URL are skipped with a warning. SHA-256
is preferred over MD5. Unclosed or multiple document roots are rejected.
Torrent `metaurl`, piece hashes, and other checksum algorithms are ignored;
declared size is parsed but is not an independent integrity constraint.

### Recursive HTTP

With `recursive-http` enabled:

```rust
let manifest = engine
    .discover_http_recursive(root_url, &options, &recursive_options)
    .await?;

let job = engine
    .add_http_recursive(root_url, options, recursive_options)
    .await?;

let aggregate = engine.recursive_job_status(&job);
println!(
    "{:?} ({}/{})",
    aggregate.state,
    aggregate.progress.completed_children,
    aggregate.progress.total_children,
);

let tracked = engine.list_recursive_jobs();
println!("tracked jobs: {}", tracked.len());

if let Some(parent) = tracked.first() {
    let mut recursive_events = engine.subscribe_recursive_jobs();
    engine.cancel_recursive_job(parent.id, false).await?;
    engine.remove_recursive_job(parent.id, false).await?;

    for _ in 0..2 {
        if let Ok(event) = recursive_events.recv().await {
            println!("recursive event: {:?}", event);
        }
    }
}
```

### Download Options

```rust
use gosh_dl::{DownloadOptions, DownloadPriority, ExpectedChecksum};

let options = DownloadOptions {
    priority: DownloadPriority::High,
    save_dir: Some(PathBuf::from("/downloads")),
    filename: Some("custom_name.zip".to_string()),
    user_agent: Some("MyApp/1.0".to_string()),
    referer: Some("https://example.com".to_string()),
    headers: vec![("Authorization".to_string(), "Bearer token".to_string())],
    cookies: Some(vec!["session=abc123".to_string()]),
    checksum: ExpectedChecksum::parse("sha256:abcd1234..."),
    mirrors: vec!["https://mirror1.example.com/file.zip".to_string()],
    max_connections: Some(8),
    max_download_speed: Some(5 * 1024 * 1024), // 5 MB/s
    max_upload_speed: Some(1024 * 1024), // 1 MB/s (torrents)
    // Torrent-specific
    selected_files: Some(vec![0, 2, 5]), // Download only specific files
    sequential: Some(true), // For streaming playback
    ..Default::default()
};
```

### Recursive HTTP

When the `recursive-http` feature is enabled, the engine also exposes `RecursiveOptions`:

```rust
use gosh_dl::RecursiveOptions;

let recursive = RecursiveOptions {
    max_depth: 4,
    include_patterns: vec!["*.txt".to_string()],
    exclude_patterns: vec!["private/*".to_string()],
    ..Default::default()
};
```

Current scope:

- crawls HTML directory/index pages and follows `<a href>` links
- same-host only by default
- constrained to the root path prefix by default
- discovered files are queued as ordinary HTTP downloads
- rolls back already-added child downloads if recursive enqueue fails partway through
- optional `fail_fast` cancels queued/active sibling child downloads after the first child failure
- persists recursive child runtime context needed for redirect-scope and fail-fast recovery
- persists tracked parent recursive jobs and restores them on restart
- exposes aggregate parent status, lifecycle methods, and a dedicated parent event stream
- propagates headers, cookies, user-agent, and referer during discovery
- discovery fetches pages concurrently, bounded by `max_discovery_concurrency` (default 4)

Current limitations:

- opt-in via the `recursive-http` Cargo feature
- not full `wget -r` parity
- no JavaScript rendering
- recursive parent jobs use a separate event stream via `subscribe_recursive_jobs()`, not the main `DownloadEvent` stream
- recursive redirect scope is enforced in discovery, child file downloads, and resumed child downloads restored from storage, but recursive jobs are still not resumable as crawls
- recursive jobs are persisted and listable, but still do not participate in the main download queue/event model as first-class parent downloads
- no persisted parent-level event or progress history beyond the tracked job record itself

### Events

```rust
use gosh_dl::DownloadEvent;

let mut events = engine.subscribe();
while let Ok(event) = events.recv().await {
    match event {
        DownloadEvent::Added { id } => println!("Added: {}", id),
        DownloadEvent::Started { id } => println!("Started: {}", id),
        DownloadEvent::Progress { id, progress } => {
            println!("{}: {:.1}% at {} KB/s",
                id,
                progress.percentage(),
                progress.download_speed / 1024
            );
        }
        DownloadEvent::StateChanged { id, old_state, new_state } => {
            println!("{}: {:?} -> {:?}", id, old_state, new_state);
        }
        DownloadEvent::Completed { id } => println!("Done: {}", id),
        DownloadEvent::Failed { id, error, retryable } => {
            eprintln!("Failed {}: {} (retryable: {})", id, error, retryable);
        }
        DownloadEvent::Paused { id } => println!("Paused: {}", id),
        DownloadEvent::Resumed { id } => println!("Resumed: {}", id),
        DownloadEvent::Removed { id } => println!("Removed: {}", id),
    }
}
```

## Configuration

```rust
use gosh_dl::{EngineConfig, HttpConfig, TorrentConfig};
use gosh_dl::config::WebSeedConfig;
use std::path::PathBuf;

let config = EngineConfig {
    download_dir: PathBuf::from("/downloads"),
    max_concurrent_downloads: 5,
    max_connections_per_download: 16,
    min_segment_size: 1024 * 1024, // 1 MB
    global_download_limit: Some(10 * 1024 * 1024), // 10 MB/s
    global_upload_limit: Some(5 * 1024 * 1024), // 5 MB/s
    user_agent: "MyApp/1.0".to_string(),
    enable_dht: true,
    enable_pex: true,
    enable_lpd: true,
    max_peers: 55,
    seed_ratio: 1.0,
    database_path: Some(PathBuf::from("/data/gosh-dl.db")),
    http: HttpConfig {
        max_retries: 8,
        read_timeout: 90,
        ..Default::default()
    },
    torrent: TorrentConfig {
        webseed: WebSeedConfig {
            enabled: true,
            max_connections: 6,
            ..Default::default()
        },
        ..Default::default()
    },
    ..Default::default()
};
```

You can also apply a replacement config at runtime with `engine.set_config(config)?;`.
Queue concurrency and global bandwidth limits are applied to the live engine when you do this.

### Persistence & Custom Storage

Setting `database_path` persists download state to the built-in SQLite storage
(`storage` feature) so downloads can resume across restarts. If you maintain your
own metadata store — or just prefer plain files — inject any implementation of
the `Storage` trait instead:

```rust
use std::sync::Arc;
use gosh_dl::{DownloadEngine, EngineConfig, FileStorage};

// aria2-control-file style JSON sidecars, one per download:
let storage = Arc::new(FileStorage::new("/data/gosh-dl-state").await?);
let engine = DownloadEngine::with_storage(EngineConfig::default(), storage).await?;
```

`FileStorage` (JSON sidecar files) and `MemoryStorage` ship with the crate and
work without the `storage` feature. To bring your own database, implement the
`Storage` trait (the `#[async_trait]` attribute is re-exported from
`gosh_dl::storage`) and pass it to `DownloadEngine::with_storage`.

After restart, previously queued downloads auto-start; downloads that were
connecting, downloading, or seeding are restored as paused and require
`resume(id)` or `resume_all()`. Completed and error states remain unchanged.
Saved HTTP segments retain their readable prefix; saved torrent metainfo
supports offline verification and reading. `MemoryStorage` retains data only
while its instance lives and does not survive process exit.

### Bandwidth Scheduling

```rust
use gosh_dl::{EngineConfig, ScheduleRule};

// Limit bandwidth during work hours (Mon-Fri, 9am-5pm)
let work_hours = ScheduleRule::weekdays(
    9,                      // start_hour
    17,                     // end_hour
    Some(1024 * 1024),      // download_limit: 1 MB/s
    None,                   // upload_limit: unlimited
);

let config = EngineConfig::default()
    .add_schedule_rule(work_hours);
```

## Building

```bash
cargo build --locked --release
cargo test --locked --all-features
cargo doc --locked --all-features --open
```

## Releasing

The release workflow runs when `Cargo.toml` or `CHANGELOG.md` changes on
`main`, or when manually dispatched on `main`. It runs the complete CI suite,
extracts the version's changelog entry, and verifies the source package before
publishing to crates.io through Trusted Publishing. Only after registry
publication succeeds does it create the GitHub tag/release and attach the
`.crate` source package.

A crate owner must configure this once under the `gosh-dl` crate's **Settings →
Trusted Publishing** on [crates.io](https://crates.io/crates/gosh-dl):

| Setting | Value |
| --- | --- |
| Provider | GitHub |
| Repository owner | `goshitsarch-eng` |
| Repository name | `gosh-dl` |
| Workflow filename | `release.yml` (not the full path) |
| Environment | Leave empty; this workflow does not use an environment |

The workflow requests a short-lived token using GitHub OIDC immediately before
publishing; the authentication action revokes it when the job finishes. No
`cargo login` or permanent GitHub secret is required. See the official
[Trusted Publishing guide](https://crates.io/docs/trusted-publishing).

After configuring the publisher, merge the version/changelog update. If an
attempt fails before authorization is configured, save the configuration and
rerun the failed jobs in GitHub Actions. If registry publication succeeded but
GitHub release creation failed, a rerun verifies the registry checksum against
the locally built package before skipping publication and completing the
GitHub release. A checksum mismatch stops the job; published versions cannot
be overwritten. Existing GitHub releases are left unchanged.

See [ROLLOUT.md](ROLLOUT.md) for rollout scope and remaining validation work.

See [technical_spec.md](technical_spec.md) for architecture details.

---

## Why an API Instead of RPC?

Traditional download managers like aria2 use JSON-RPC for external communication. This works well for standalone tools, but creates friction when embedding download functionality into applications:

**With RPC (aria2 approach):**
```
Your App → Serialize JSON → HTTP/WebSocket → aria2 Process → Parse JSON → Execute
         ← Parse JSON    ← HTTP/WebSocket  ←              ← Serialize JSON ← Result
```

**With native API (gosh-dl approach):**
```
Your App → engine.add_http(url, opts) → Result
```

### Benefits of the API Approach

- **Zero serialization overhead**: No JSON encoding/decoding on every call. Function arguments pass directly through memory.
- **Compile-time guarantees**: The Rust compiler catches type mismatches, missing parameters, and invalid states before your code runs. RPC errors only surface at runtime.
- **Native error handling**: Use `?` operator, pattern matching on `Result`, and standard Rust error propagation. No parsing error strings from JSON responses.
- **No process coordination**: No need to spawn aria2, monitor if it crashed, restart it, or manage its lifecycle. The engine lives in your process.
- **Shared memory space**: Progress callbacks, event streams, and status queries happen in-process. No IPC latency or message queue bottlenecks.
- **Single deployment artifact**: Ship one binary. No bundling platform-specific aria2 executables or dealing with PATH issues.
- **IDE integration**: Autocomplete, go-to-definition, inline docs all work. RPC calls are opaque strings to your editor.

---

## Comparison with aria2

gosh-dl was designed as a native Rust alternative to [aria2](https://aria2.github.io/), the popular C++ download utility. While aria2 is excellent as a standalone tool, embedding it in applications requires spawning an external process and communicating via JSON-RPC.

| Aspect | aria2 | gosh-dl |
|--------|-------|---------|
| **Integration** | External process + JSON-RPC | Native library calls |
| **Deployment** | Bundle platform binaries | Single Rust crate |
| **Type Safety** | JSON strings | Rust types with compile-time checks |
| **Error Handling** | Parse JSON responses | Native `Result<T, E>` |
| **Process Management** | Handle lifecycle, crashes | None required |
| **Memory** | Separate process | Shared with your app |

### Migration Guide

| aria2 RPC | gosh-dl |
|-----------|---------|
| `aria2.addUri(urls)` | `engine.add_http(url, opts)` |
| `aria2.addTorrent(torrent)` | `engine.add_torrent(bytes, opts)` |
| `aria2.pause(gid)` | `engine.pause(id)` |
| `aria2.unpause(gid)` | `engine.resume(id)` |
| `aria2.pauseAll()` | `engine.pause_all()` |
| `aria2.unpauseAll()` | `engine.resume_all()` |
| `aria2.remove(gid)` | `engine.cancel(id, false)` |
| `aria2.tellStatus(gid)` | `engine.status(id)` |
| `aria2.tellActive()` | `engine.active()` |
| `aria2.tellWaiting()` | `engine.waiting()` |
| `aria2.tellStopped()` | `engine.stopped()` |
| `aria2.getGlobalStat()` | `engine.global_stats()` |
| `aria2.changeOption(gid, {priority})` | `engine.set_priority(id, priority)` |

---

## FAQ

### Why not just use aria2?

aria2 is a battle-tested download utility and remains an excellent choice for many use cases. Use aria2 if:

- You need a standalone command-line tool
- You're scripting downloads from shell or other languages
- You want a mature, widely-deployed solution with years of production use

Use gosh-dl if:

- You're building a Rust application and want download functionality as a library
- You need tight integration without IPC overhead
- You want compile-time type safety and native async/await
- You prefer not to bundle and manage external binaries
- You need direct access to download state without polling JSON-RPC

Both tools support similar feature sets (multi-connection HTTP, BitTorrent, DHT, etc.). The difference is architectural: aria2 is a standalone process you communicate with, gosh-dl is a library you call directly.

### Is there a CLI?

A standalone `gosh-dl` CLI application is now available and can be found here: [gosh-dl-cli](https://github.com/goshitsarch-eng/gosh-dl-cli). It allows command-line access to all engine features for users who prefer terminal workflows or need to script downloads without writing Rust code.

### What Rust version is required?

gosh-dl requires Rust 1.85+ for async trait support.

### Does gosh-dl work on Windows?

Yes. gosh-dl supports Linux, macOS, and Windows. Platform-specific code handles differences in file handling, network interfaces, and path conventions.

---

## License

MIT License - see [LICENSE](LICENSE) for details.

## Acknowledgments

- Built with [Tokio](https://tokio.rs/) for async runtime
- Uses [mainline](https://crates.io/crates/mainline) for DHT support

### Adding work without starting transfers

Set `DownloadOptions { start_paused: true, ..Default::default() }` to create
HTTP, torrent, or magnet downloads in `DownloadState::Paused`. Call
`engine.resume(id).await?` to start them later. Recursive HTTP jobs still fetch
directory listings during discovery, but their child file transfers remain
paused. Options and torrent metadata are retained for resume; persistent
storage is required to resume across process exits.
