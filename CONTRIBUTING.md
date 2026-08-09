# Contributing to horsie

Thanks for wanting to help. Issues, bug reports, and pull requests are all
welcome.

## Contributor License Agreement

Before your first pull request can be merged you need to sign the
[CLA](https://github.com/blossomstack/.github/blob/main/CLA.md). A bot comments on your PR with a link; signing takes a moment in
the browser and covers all your future contributions, here and in every other
blossomstack repository.

**Why?** horsie is dual-licensed Apache-2.0 / MIT today. The CLA keeps the door
open to changing that later — for example to fund the project's development —
without needing to track down every past contributor for permission. You keep
the copyright to everything you write.

## Before you open a PR

Run the same gate CI runs:

```bash
make check   # cargo fmt --check, clippy -D warnings, cargo test --workspace
```

Wire and protocol types are generated with
[fluorite](https://github.com/blossomstack/fluorite) from the schemas under
`crates/models/fluorite/` — edit the schema, not the generated Rust or TypeScript.
Production code denies `unwrap`, `expect`, `panic`, and wildcard match arms;
tests opt out per file.

`CLAUDE.md` has the full design philosophy and code conventions. Please read it
before a non-trivial change — it explains why the codebase looks the way it
does.

## Documentation

The docs at [docs.horsie.dev](https://docs.horsie.dev) are built from `docs/`
in this repository, so a change that alters behaviour changes its pages in the
same PR. `make docs` previews them locally and `make docs-check` runs the same
gate CI does.

How they are structured, what each kind of page may contain, and the wording
rules CI enforces are in
[Writing docs](https://docs.horsie.dev/contributing/writing-docs/).

## Releasing

Which crates go to crates.io is not a per-crate judgement call. The published
set is exactly the dependency closure of the two installable binaries, `horsie`
and `horsie-runtime`; `scripts/check-publish-surface.sh` enforces that in CI.
`horsie-server` is deliberately outside it — it ships as a release tarball
binary and a container image, never through crates.io. If you add a crate,
`publish = false` unless one of those two binaries links it.

### Never reuse a version already on crates.io

`publish.yml` skips any crate whose version is already on the registry, so a tag
that reuses a published version silently keeps the *old* published crate and
then builds its dependents against it. Today `horsie-models 0.1.6` is on
crates.io from the pre-rename layout and no longer matches this tree, so a tag
of `v0.1.6` would skip it and fail while verifying `horsie-support` against it.
The next tag must be `v0.1.7` or later, with every workspace crate bumped to
match — `version-guard` checks that before anything is published.

### The first tag after a crate rename

`publish.yml` authenticates by OIDC, but trusted publishing can only be
configured for a crate that already exists on crates.io. A newly named crate
therefore needs the `CARGO_REGISTRY_TOKEN` secret for its first publish only.

`horsie-support` and `horsie-runtime-host` are both in this state. Before
tagging:

1. Check whether the `CARGO_REGISTRY_TOKEN` repository secret still exists. If
   not, mint a scoped publish token on crates.io and add it back.
2. Tag and let the publish run create both crates.
3. On crates.io, configure trusted publishing for each new crate: repository
   `blossomstack/horsie`, workflow `publish.yml`.
4. Delete the secret again.

The auth step in `publish.yml` already has `continue-on-error: true` and falls
through to the secret, so no workflow change is needed.

## Licence of contributions

Unless you state otherwise, contributions are submitted under the terms of the
[CLA](https://github.com/blossomstack/.github/blob/main/CLA.md) and distributed under [Apache-2.0](LICENSE-APACHE) and
[MIT](LICENSE-MIT).
