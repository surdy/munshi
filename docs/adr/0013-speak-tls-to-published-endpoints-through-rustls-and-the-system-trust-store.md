# Speak TLS to published endpoints through rustls and the system trust store

The shared blocking HTTP/1.1 client was deliberately plain-HTTP over `std::net` (ADR 0006):
both peers were trusted localhost/LAN daemons. Patwari is now also published at
`https://patwari.clusterfault.com` through quadhost's Caddy — TLS terminated at the proxy,
certificates issued by Let's Encrypt over DNS-01, the name resolving only to LAN/tailnet
addresses — and that is the natural endpoint for any machine outside the trusted subnet.
Until now such machines needed an SSH tunnel, a manual dependency that quietly queues uploads
when it is down (munshi issue #35).

TLS is a stream wrapper, not a client redesign. `https://` endpoints wrap the same
one-fresh-connection-per-request `TcpStream` in rustls; everything else — bounded reads,
`Connection: close`, EOF-delimited responses, request independence that the retry/park state
machine leans on — is untouched. The per-request handshake this implies was measured against
the actual workloads and accepted: one extra round-trip (~1–3 ms on LAN/tailnet) against
requests that move megabytes is 2–3% on the request-dense paths (chunk uploads, the archive
walk), and a process-wide shared `ClientConfig` gives TLS 1.3 session resumption to every
request after an invocation's first. Connection reuse was rejected: it would replace
EOF framing with stateful response framing and stale-socket handling, and munshi's
hook-driven processes are too short-lived for a pool to pay for that.

The backend is rustls on the `ring` provider with roots from `rustls-native-certs`:
identical behavior on macOS and Linux, no dependence on the platform's deprecated or
system TLS libraries, no cmake/asm build surprises. Verification is standard and
policy-free — full chain and name verification against the system trust store, nothing
else. No pinning (Let's Encrypt leaves rotate), no `--insecure` escape hatch, no CA-bundle
knob: the system trust store is the extension point, so a future private CA (for example
IP-SAN certificates from Caddy's internal CA) works by trusting that CA on the machine,
not by teaching munshi TLS policy. The client encodes no opinion about what a certificate
may authorize; DNS names and IP literals both map to their rustls `ServerName` forms and
whatever the presented certificate covers is accepted. Plain `http://` remains fully
supported for tunnels, loopback tests, and trusted-LAN addresses — the trusted-network
model, not the client, governs where it is appropriate.

TLS reads are strict about truncation: a peer that closes without `close_notify` is a
transport error, never a silently shortened body. Artifact downloads are additionally
hash-verified a layer up, so tolerating the missing alert would buy nothing but vaguer
failures. The `Host` header now omits the scheme-default port (80/443), matching virtual-host
convention; explicit non-default ports are still sent. IPv6 bracket authorities remain
unsupported in the endpoint parser, a pre-existing limitation deliberately left out of scope.
The TLS path is gated by endpoint-parsing and `ServerName` unit tests plus the live
acceptance walk (`verify-archive-parse --all --endpoint https://…`) rather than a
self-signed-certificate harness, which would exist only to exercise plumbing whose sole
consumer is the test.
