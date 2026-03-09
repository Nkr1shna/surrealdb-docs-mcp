# surrealdb-docs-mcp

A minimal MCP server in Rust that exposes two tools:

- `search_docs` for ranked SurrealDB doc search
- `fetch_doc` for retrieving the full content of a doc page

Current behavior:

- Uses the official Rust MCP SDK: [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)
- Uses the same hosted search index as the docs site for ranked search results
- Reads full doc content from a cached local checkout of [`surrealdb/docs.surrealdb.com`](https://github.com/surrealdb/docs.surrealdb.com)

Startup behavior:

- On first run the server clones a shallow sparse checkout of the docs repo (only `src/content`) into the platform cache directory
- On subsequent runs it checks `.git/FETCH_HEAD` in that checkout
- If `FETCH_HEAD` is newer than 6 hours, it skips `git pull` and uses the cached docs repo
- If `FETCH_HEAD` is older than 6 hours, or missing, it runs `git pull --ff-only --depth 1`
- The clone/pull runs in the background so the server starts serving immediately
- If it fails, a warning is logged to stderr and the server continues with whatever is cached

## Tools

### `search_docs`

Request shape:

```json
{
  "query": "embedded",
  "limit": 5
}
```

Response shape:

```json
{
  "query": "embedded",
  "count": 2,
  "results": [
    {
      "title": "Embedding SurrealDB",
      "description": "In this section, you will find detailed instructions...",
      "hostname": "main--surrealdb-docs.netlify.app",
      "path": "/docs/surrealdb/embedding",
      "url": "https://surrealdb.com/docs/surrealdb/embedding",
      "score": 201.06226
    }
  ]
}
```

### `fetch_doc`

Request shape:

```json
{
  "url": "/docs/surrealdb/embedding"
}
```

The `url` field can be either:

- the relative `path` returned by `search_docs`
- the absolute `url` returned by `search_docs`

Response shape:

```json
{
  "requested_url": "/docs/surrealdb/embedding",
  "resolved_url": "https://surrealdb.com/docs/surrealdb/embedding",
  "title": "Embedding SurrealDB",
  "description": "In this section, you will find detailed instructions...",
  "content_format": "mdx",
  "source_path": "/Users/(username)/Library/Caches/surrealdb-docs-mcp/docs.surrealdb.com/src/content/doc-surrealdb/embedding/index.mdx",
  "content": "---\n..."
}
```

Implementation note:

- `search_docs` calls the hosted docs search API and maps its ranked results into MCP output.
- `fetch_doc` resolves the docs URL to a source file inside the cached repo.
- It currently supports the main docs collections and SDK routes used by the SurrealDB docs site.

## Development

Build:

```bash
cargo build
```

Run over stdio (clones the docs repo on first run):

```bash
cargo run
```

Run tests:

```bash
cargo test
```

The two filesystem integration tests are ignored by default. Run them with the docs repo present:

```bash
cargo test -- --include-ignored
```

## Environment

These are optional overrides:

- `SURREALDB_DOCS_SITE_URL`
  - Defaults to `https://surrealdb.com`
- `SURREALDB_DOCS_SEARCH_API_URL`
  - Defaults to `https://surrealdb.com/api/docs/search`
- `SURREALDB_DOCS_REPO_GIT_URL`
  - Defaults to `https://github.com/surrealdb/docs.surrealdb.com.git`
- `SURREALDB_DOCS_REPO_PATH`
  - Overrides the local docs checkout path entirely
- `SURREALDB_DOCS_REPO_REFRESH_MAX_AGE_SECS`
  - Maximum allowed age of `.git/FETCH_HEAD` before startup refreshes the repo
  - Defaults to `21600` seconds (6 hours)

Default docs checkout location when `SURREALDB_DOCS_REPO_PATH` is not set:

- Linux and WSL
  - `$XDG_CACHE_HOME/surrealdb-docs-mcp/docs.surrealdb.com` when `XDG_CACHE_HOME` is set
  - Otherwise `~/.cache/surrealdb-docs-mcp/docs.surrealdb.com`
- macOS
  - `$XDG_CACHE_HOME/surrealdb-docs-mcp/docs.surrealdb.com` when `XDG_CACHE_HOME` is set
  - Otherwise `~/Library/Caches/surrealdb-docs-mcp/docs.surrealdb.com`
- Windows
  - `%LOCALAPPDATA%\surrealdb-docs-mcp\docs.surrealdb.com`

## Example MCP Config

```json
{
  "mcpServers": {
    "surrealdb-docs": {
      "command": "cargo",
      "args": ["run", "--quiet"],
      "cwd": "/path/to/surrealdb-docs-mcp"
    }
  }
}
```
