# Orange SDK patch

This directory vendors `ldk-node` commit
`8a5426044bdcae6369d7a847697c6143676e2df5`, matching the revision previously pinned in
`orange-sdk/Cargo.toml`.

Orange SDK carries one patch in `src/io/vss_store.rs`: VSS stores always use the current V1 key and
encryption schema. Legacy V0 stores are intentionally unsupported, so wallet startup skips the
schema-version request and fallback probe.
