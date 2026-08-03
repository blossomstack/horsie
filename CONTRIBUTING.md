# Contributing to horsie

Thanks for wanting to help. Issues, bug reports, and pull requests are all
welcome.

## Contributor License Agreement

Before your first pull request can be merged you need to sign the
[CLA](CLA.md). A bot will comment on your PR with instructions; signing is a
one-line comment and covers all your future contributions.

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
[fluorite](https://github.com/zhxiaogg/fluorite) from the schemas under
`models/fluorite/` — edit the schema, not the generated Rust or TypeScript.
Production code denies `unwrap`, `expect`, `panic`, and wildcard match arms;
tests opt out per file.

`CLAUDE.md` has the full design philosophy and code conventions. Please read it
before a non-trivial change — it explains why the codebase looks the way it
does.

## Licence of contributions

Unless you state otherwise, contributions are submitted under the terms of the
[CLA](CLA.md) and distributed under [Apache-2.0](LICENSE-APACHE) and
[MIT](LICENSE-MIT).
