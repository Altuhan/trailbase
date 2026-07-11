# Tech-improvements refactoring backlog

Working file driving an iterative implementation loop on branch
`claude/trailbase-tech-improvements-aq05eu`. One unchecked item per iteration:
implement → verify → commit → check off (`[x]` done, `[!] reason` blocked).
Remove this file before opening a PR.

## Package 1 — quick wins

- [ ] 1. Fix `Makefile` `publish_crates`: replace stale `trailbase-sqlean`/`trailbase-js`
      (leftovers of the removed V8 runtime) with the current path-dependency closure of
      the `trailbase` crate in publish order (build → assets/auth-config/auth-ui →
      qs/refinery/extension/schema/sqlvalue/sqlite/reactive → wasm-common/wasm-runtime-host/
      wasm-runtime-axum → trailbase). `crates/cli` is `publish = false` — keep excluded.
      Verify: every `-p` name exists in `cargo metadata`; make syntax intact.
- [ ] 2. CI: replace archived `actions-rs/toolchain@v1` with `dtolnay/rust-toolchain` (same
      1.95.0 + rustfmt/clippy components) in `.github/actions/setup_build/action.yml` and
      4 jobs in `.github/workflows/release.yml` (~lines 167, 209, 251, 299).
      Verify: YAML parses; inputs match dtolnay action (`toolchain:`, `components:`, `targets:`).
- [ ] 3. CI: add `Swatinem/rust-cache@v2` to `setup_build/action.yml` after toolchain setup
      (5 test.yml jobs currently build the workspace cold). Do NOT add to release jobs.
      Verify: YAML parses.
- [ ] 4. Replace `lazy_static` with `std::sync::LazyLock`: crates/core/src/{config,metadata}.rs,
      crates/core/src/admin/logs/list_logs.rs, crates/core/src/auth/oauth/providers/*.rs (×10),
      crates/schema/src/metadata.rs; drop the dependency from crates/core/Cargo.toml and
      crates/schema/Cargo.toml. Do NOT touch `async-trait` (OAuthProvider/email traits are
      dyn-dispatched; AFIT does not apply).
      Verify: `cargo clippy -p trailbase -p trailbase-schema --no-deps`; `cargo test -p trailbase-schema`.
- [ ] 5. HTTP response compression: add tower-http features `compression-br`, `compression-gzip`,
      `compression-zstd` in crates/core/Cargo.toml; add `CompressionLayer::new()` to the main
      router in `build_main_router` (crates/core/src/server/mod.rs). Default predicate skips
      `text/event-stream` (SSE subscriptions) and <32B bodies.
      Verify: `cargo test -p trailbase`; manual: run `trail`, `curl -sS -H 'Accept-Encoding: gzip' -i /api/healthcheck`
      and an admin-UI asset show `content-encoding`; SSE endpoint does not.

## Package 2 — supply chain / CI quality

- [ ] 6. Add `deny.toml` + a `cargo-deny` job (`EmbarkStudios/cargo-deny-action@v2`) to
      `.github/workflows/test.yml`: advisories (RUSTSEC), licenses allowlist
      (MIT/Apache-2.0/BSD-2/BSD-3/ISC/Zlib/MPL-2.0/Unicode-3.0/CC0-1.0 + exceptions tuned
      from a real run), bans (multiple-versions = warn), sources (allow github.com/ignatz).
      Verify: local `cargo deny check` passes.
- [ ] 7. Add `.github/dependabot.yml`: cargo (weekly, grouped minor/patch), npm (weekly, grouped,
      workspace directories), github-actions, gitsubmodule.
      Verify: YAML parses, matches dependabot v2 schema.
- [ ] 8. Release integrity: generate `SHA256SUMS` for artifacts and add
      `actions/attest-build-provenance` (with `id-token: write`, `attestations: write`
      permissions) to `.github/workflows/release.yml`.
      Verify: static only (real run needs a tag) — mark as statically-verified.
- [ ] 9. CI: switch test jobs to `cargo-nextest` (`taiki-e/install-action` + `cargo nextest run`)
      where `cargo test` runs in test.yml / pre-commit CI path.
      Verify: local `cargo nextest run -p trailbase-sqlite -p trailbase-qs`.

## Package 3 — finalization

- [ ] 10. Full verification: `pnpm i`, `cargo clippy --workspace --features=geos,otel,pg,wasm --no-deps`,
      `cargo test --workspace --features=geos,otel,wasm` (skip pg-test: needs external Postgres),
      `cargo fmt` per repo config.
- [ ] 11. CHANGELOG.md entry (repo style: bullet list, no version bump).
- [ ] 12. Final push to `claude/trailbase-tech-improvements-aq05eu` + user summary
      (done / blocked / follow-ups). Follow-ups intentionally NOT implemented: channel-crate
      consolidation, PGO/BOLT, wasmtime pooling allocator, protobuf-es migration, Prometheus
      exporter, Litestream-style replication, at-rest encryption.
