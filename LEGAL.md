# Legal

This crate is MIT-licensed. That applies to the code in this repository.

It does not grant rights to the content of `surrealdb/docs.surrealdb.com`.

Current boundary:

- `search_docs` calls SurrealDB's hosted search endpoint.
- `fetch_doc` reads from a user-provided local checkout of the docs repo.
- `vendor/**` is excluded from the published crate.

Safe position:

- publish this crate under MIT
- do not bundle, mirror, or relicense the official docs content
- do not claim rights over the official search index or documentation text

As of 2026-03-09, the upstream docs repo does not appear to publish a clear repo-level license. If you want to redistribute the docs content, get explicit permission from SurrealDB first.

This is a practical packaging note, not legal advice.
