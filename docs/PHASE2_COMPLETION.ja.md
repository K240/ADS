# Phase 2 Completion Notes

## Status

Phase 2 is complete as a local USD/Houdini integration milestone.

This phase covers:

- C++ OpenUSD `ArResolver` prototype for `ads://` URI resolution.
- Houdini build script and package example.
- `ads-deps` USD dependency preflight utility.
- Texture-aware hash cache resolution for large texture assets.
- Houdini Solaris USD ROP `ADS Managed Publish` output processor.
- Public publish validation and registration CLI.

## Completed Interfaces

Resolver:

```powershell
ads resolve --store D:\store --workspace D:\workspace ads://char/hero/model/hero.usd
```

Dependency preflight:

```powershell
uv run ads-deps D:\shots\shot010\shot.usda --store D:\store --workspace D:\workspace
```

Houdini USD ROP public publish:

```powershell
ads publish validate `
  --store D:\store `
  --public-root D:\public `
  --category char `
  --asset-code hero `
  --department model `
  --version v003

ads publish register `
  --store D:\store `
  --public-root D:\public `
  --category char `
  --asset-code hero `
  --department model `
  --version v003
```

Texture cache resolve:

```text
ads://char/hero/texture/maps/body.1001.tx
  -> <workspace>/.ads-cache/sha256/<prefix>/<hash>.tx
```

## Phase 2 Boundaries

The Phase 2 resolver is read-only and local-file based. It does not mutate workspace state or run `pull` implicitly.

Remote object direct read through a custom USD `ArAsset` is intentionally deferred to Phase 3. Phase 2 can return remote object URLs through `ads resolve --mode remote`, but the C++ resolver still opens filesystem paths through `ArFilesystemAsset`.

Binary USD reference validation is also deferred to the OpenUSD/Houdini preflight path. The CLI `publish validate` command validates text USD files directly and warns when binary layers are skipped.

## Verification

Expected verification for this milestone:

- `cargo fmt --check`
- `uv run python -m unittest discover -s tests`
- `git diff --check`
- Houdini resolver smoke test when Houdini is available

On the current Windows development machine, `cargo test` requires `libclang` for `librocksdb-sys` bindgen. If `LIBCLANG_PATH` is unset, Rust tests stop before compiling ADS code.
