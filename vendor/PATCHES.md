# Local gosh-dl 0.6.3 patches

The source in `gosh-dl/` is the upstream MIT-licensed 0.6.3 crate. Its `LICENSE`
is preserved. FFDownload applies two transport compatibility changes:

- HEAD metadata and resume probes send `Connection: close`. A tested media CDN
  closes its HEAD connection without a TLS close notification; reusing that
  socket for the first ranged GET produced an immediate `UnexpectedEof`.
  GET requests retain their normal connection pooling and parallel ranges.
- Request errors are retryable only when their typed source chain contains an
  `std::io::Error` with `UnexpectedEof`, `ConnectionReset`, `ConnectionAborted`,
  `BrokenPipe`, or `NotConnected`. The existing finite retry budget still applies.
  Builder errors, certificate failures, protocol errors without those I/O causes,
  and HTTP status handling are unchanged. Error text is not used for classification.

Modified upstream files: `src/error.rs`, `src/http/mod.rs`,
`src/http/segment.rs`, and `src/http/resume.rs`.

Upstream package: <https://crates.io/crates/gosh-dl/0.6.3>, archive SHA-256
`c4204babb283aa7d1cb639f2668b21db69dad1221d41cb404b960e069265a379`.
The API, storage schema, and dependency versions remain unchanged.

`tests/stale_connection_flow.rs` reproduces the TLS response-header interruption,
checks successful segmented retry and file integrity, exercises retry exhaustion,
and confirms that 401/403 remain permanent failures. The existing authenticated
resume test also checks that `Accept-Encoding: identity` is sent exactly once.
