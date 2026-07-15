# Orange SDK patch

This directory vendors `ldk-node` commit
`8a5426044bdcae6369d7a847697c6143676e2df5`, matching the revision previously pinned in
`orange-sdk/Cargo.toml`.

Orange SDK carries one compatibility patch in `src/io/vss_store.rs`: schema setup and subsequent
asynchronous VSS operations share a Bitreq HTTP pool. Both run on the store's isolated runtime, so
the schema connection can be reused instead of opening a second connection (and, for HTTPS, doing a
second TLS handshake) during wallet startup.
