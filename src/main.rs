use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

use reqwest::Client;
use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars,
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use url::Url;

const DEFAULT_DOCS_SITE_URL: &str = "https://surrealdb.com";
const DEFAULT_DOCS_SEARCH_API_URL: &str = "https://surrealdb.com/api/docs/search";
const DEFAULT_DOCS_REPO_GIT_URL: &str = "https://github.com/surrealdb/docs.surrealdb.com.git";
const DEFAULT_APP_DIR_NAME: &str = "surrealdb-docs-mcp";
const DEFAULT_DOCS_REPO_DIR_NAME: &str = "docs.surrealdb.com";
const DEFAULT_DOCS_REPO_REFRESH_MAX_AGE_SECS: u64 = 6 * 60 * 60;
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 25;
const SEARCH_REQUEST_TIMEOUT_SECS: u64 = 10;
// Clone needs more headroom than a pull on a slow connection.
const DOCS_REPO_SETUP_TIMEOUT_SECS: u64 = 120;

const COLLECTION_ROUTES: &[(&str, &str)] = &[
    ("doc-sdk-javascript-1x", "1.x/sdk/javascript"),
    ("doc-sdk-python-1x", "1.x/sdk/python"),
    ("doc-sdk-dotnet", "sdk/dotnet"),
    ("doc-sdk-golang", "sdk/golang"),
    ("doc-sdk-java", "sdk/java"),
    ("doc-sdk-javascript", "sdk/javascript"),
    ("doc-sdk-php", "sdk/php"),
    ("doc-sdk-python", "sdk/python"),
    ("doc-sdk-rust", "sdk/rust"),
    ("doc-surrealdb", "surrealdb"),
    ("doc-cloud", "cloud"),
    ("doc-surrealist", "surrealist"),
    ("doc-surrealml", "surrealml"),
    ("doc-surrealql", "surrealql"),
    ("doc-integrations", "integrations"),
    ("doc-tutorials", "tutorials"),
    ("labs-items", "labs"),
];

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchDocsRequest {
    #[schemars(description = "Search terms to send to the SurrealDB documentation index.")]
    query: String,
    #[schemars(
        description = "Maximum number of ranked hits to return. Defaults to 10 and caps at 25."
    )]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FetchDocRequest {
    #[schemars(
        description = "Absolute SurrealDB docs URL or relative path returned by search_docs, for example /docs/surrealdb/embedding."
    )]
    url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchDocsResponse {
    query: String,
    count: usize,
    results: Vec<SearchDocsResult>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FetchDocResponse {
    requested_url: String,
    resolved_url: String,
    title: String,
    description: Option<String>,
    content_format: String,
    /// Absolute path to the source file in the local docs repo cache. Intentionally
    /// included so clients can open the file directly; only suitable for local use.
    source_path: String,
    content: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchDocsResult {
    title: String,
    description: String,
    hostname: String,
    path: String,
    url: String,
    score: f64,
}

#[derive(Debug, Deserialize)]
struct DocsSearchApiResult {
    title: String,
    description: String,
    hostname: String,
    score: f64,
    url: String,
}

#[derive(Clone)]
struct SurrealDocsServer {
    docs_repo_path: PathBuf,
    docs_site_url: Url,
    docs_search_api_url: Url,
    search_client: Client,
    tool_router: ToolRouter<Self>,
}

impl SurrealDocsServer {
    fn new() -> Result<Self, String> {
        let docs_site_url = env::var("SURREALDB_DOCS_SITE_URL")
            .unwrap_or_else(|_| DEFAULT_DOCS_SITE_URL.to_string());
        let docs_search_api_url = env::var("SURREALDB_DOCS_SEARCH_API_URL")
            .unwrap_or_else(|_| DEFAULT_DOCS_SEARCH_API_URL.to_string());
        let docs_repo_path = docs_repo_path_from_env()?;

        let docs_site_url = Url::parse(&docs_site_url)
            .map_err(|error| format!("invalid SURREALDB_DOCS_SITE_URL: {error}"))?;
        let docs_search_api_url = Url::parse(&docs_search_api_url)
            .map_err(|error| format!("invalid SURREALDB_DOCS_SEARCH_API_URL: {error}"))?;

        Ok(Self {
            docs_repo_path,
            docs_site_url,
            docs_search_api_url,
            search_client: Client::builder()
                .timeout(Duration::from_secs(SEARCH_REQUEST_TIMEOUT_SECS))
                .build()
                .map_err(|error| format!("failed to create HTTP client: {error}"))?,
            tool_router: Self::tool_router(),
        })
    }

    async fn fetch_search_hits(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchDocsResult>, String> {
        let hostname = docs_search_hostname(
            self.docs_site_url
                .host_str()
                .ok_or_else(|| "configured docs site URL is missing a host".to_string())?,
        );
        let results = self
            .search_client
            .get(self.docs_search_api_url.clone())
            .query(&[("hostname", hostname), ("query", query)])
            .send()
            .await
            .map_err(|error| format!("failed to query docs search API: {error}"))?
            .error_for_status()
            .map_err(|error| format!("docs search API returned an error: {error}"))?
            .json::<Vec<DocsSearchApiResult>>()
            .await
            .map_err(|error| format!("failed to decode docs search API response: {error}"))?;

        results
            .into_iter()
            .take(limit)
            .map(|result| map_search_result(&self.docs_site_url, result))
            .collect()
    }

    fn fetch_doc_from_repo(&self, requested_url: &str) -> Result<FetchDocResponse, String> {
        let doc_url = normalize_doc_url(&self.docs_site_url, requested_url)?;
        let source_path = resolve_doc_source_path(&self.docs_repo_path, &doc_url)?;
        let content = fs::read_to_string(&source_path)
            .map_err(|error| format!("failed to read {}: {error}", source_path.display()))?;
        let title = extract_frontmatter_value(&content, "title")
            .or_else(|| extract_heading_title(&content))
            .unwrap_or_else(|| {
                source_path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("Untitled document")
                    .to_string()
            });
        let description = extract_frontmatter_value(&content, "description");
        let content_format = source_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("text")
            .to_string();
        let source_path = source_path
            .canonicalize()
            .unwrap_or(source_path)
            .display()
            .to_string();

        Ok(FetchDocResponse {
            requested_url: requested_url.to_string(),
            resolved_url: doc_url.to_string(),
            title,
            description,
            content_format,
            source_path,
            content,
        })
    }
}

#[tool_router(router = tool_router)]
impl SurrealDocsServer {
    #[tool(
        name = "search_docs",
        description = "Search SurrealDB documentation and return ranked results with URLs. Use this first to find relevant pages, then pass a returned URL or path to fetch_doc to get the full content."
    )]
    async fn search_docs(
        &self,
        Parameters(request): Parameters<SearchDocsRequest>,
    ) -> Result<Json<SearchDocsResponse>, String> {
        let query = normalize_query(&request.query)?;
        let results = self
            .fetch_search_hits(query, effective_limit(request.limit))
            .await?;

        Ok(Json(SearchDocsResponse {
            query: query.to_string(),
            count: results.len(),
            results,
        }))
    }

    #[tool(
        name = "fetch_doc",
        description = "Retrieve the full markdown content of a SurrealDB documentation page. Accepts either an absolute URL (https://surrealdb.com/docs/...) or a relative path (/docs/...) returned by search_docs."
    )]
    async fn fetch_doc(
        &self,
        Parameters(request): Parameters<FetchDocRequest>,
    ) -> Result<Json<FetchDocResponse>, String> {
        let requested_url = normalize_query(&request.url)?;
        Ok(Json(self.fetch_doc_from_repo(requested_url)?))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SurrealDocsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "To answer SurrealDB questions: (1) call search_docs with relevant keywords to find matching pages, (2) call fetch_doc with the URL from the results to read the full content. Prefer fetch_doc over summarizing search snippets.",
            )
    }
}

fn docs_search_hostname(hostname: &str) -> &str {
    match hostname {
        "surrealdb.com"
        | "www.surrealdb.com"
        | "docs.surrealdb.com"
        | "surrealdb-docs.netlify.app"
        | "localhost" => "main--surrealdb-docs.netlify.app",
        other => other,
    }
}

fn map_search_result(
    docs_site_url: &Url,
    result: DocsSearchApiResult,
) -> Result<SearchDocsResult, String> {
    let path = normalize_search_result_path(&result.url)?;

    Ok(SearchDocsResult {
        title: result.title,
        description: result.description,
        hostname: result.hostname,
        path: path.clone(),
        url: build_docs_url(docs_site_url, &path),
        score: result.score,
    })
}

fn normalize_search_result_path(value: &str) -> Result<String, String> {
    let value = normalize_query(value)?;

    let path = match Url::parse(value) {
        Ok(url) => url.path().to_string(),
        Err(_) => value.to_string(),
    };

    if !path.starts_with("/docs/") {
        return Err(format!("search result URL is not a docs path: {value}"));
    }

    Ok(path)
}

fn normalize_query(query: &str) -> Result<&str, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("query must not be empty".to_string());
    }

    Ok(query)
}

fn docs_repo_path_from_env() -> Result<PathBuf, String> {
    match env::var_os("SURREALDB_DOCS_REPO_PATH") {
        Some(path) if path.is_empty() => {
            Err("SURREALDB_DOCS_REPO_PATH must not be empty".to_string())
        }
        Some(path) => Ok(PathBuf::from(path)),
        None => default_docs_repo_path(),
    }
}

fn default_docs_repo_path() -> Result<PathBuf, String> {
    Ok(default_cache_home()?
        .join(DEFAULT_APP_DIR_NAME)
        .join(DEFAULT_DOCS_REPO_DIR_NAME))
}

fn default_cache_home() -> Result<PathBuf, String> {
    let home_dir = env_path("HOME").or_else(|| env_path("USERPROFILE"));
    let xdg_cache_home = env_path("XDG_CACHE_HOME");
    let local_app_data = env_path("LOCALAPPDATA");

    default_cache_home_for(
        env::consts::OS,
        home_dir.as_deref(),
        xdg_cache_home.as_deref(),
        local_app_data.as_deref(),
    )
}

fn default_cache_home_for(
    target_os: &str,
    home_dir: Option<&Path>,
    xdg_cache_home: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Result<PathBuf, String> {
    if target_os != "windows" {
        if let Some(xdg_cache_home) = xdg_cache_home {
            if !xdg_cache_home.is_absolute() {
                return Err("XDG_CACHE_HOME must be an absolute path".to_string());
            }

            return Ok(xdg_cache_home.to_path_buf());
        }
    }

    match target_os {
        "macos" => Ok(required_home_dir(home_dir)?.join("Library").join("Caches")),
        "windows" => local_app_data.map(Path::to_path_buf).ok_or_else(|| {
            "LOCALAPPDATA is not set and SURREALDB_DOCS_REPO_PATH was not provided".to_string()
        }),
        _ => Ok(required_home_dir(home_dir)?.join(".cache")),
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_duration_secs(key: &str) -> Result<Option<Duration>, String> {
    match env::var(key) {
        Ok(value) => {
            let secs = value
                .trim()
                .parse::<u64>()
                .map_err(|error| format!("invalid {key}: {error}"))?;
            Ok(Some(Duration::from_secs(secs)))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{key} must be valid unicode")),
    }
}

fn required_home_dir(home_dir: Option<&Path>) -> Result<&Path, String> {
    home_dir
        .ok_or_else(|| "HOME is not set and SURREALDB_DOCS_REPO_PATH was not provided".to_string())
}

fn docs_repo_refresh_max_age() -> Result<Duration, String> {
    Ok(
        env_duration_secs("SURREALDB_DOCS_REPO_REFRESH_MAX_AGE_SECS")?
            .unwrap_or(Duration::from_secs(DEFAULT_DOCS_REPO_REFRESH_MAX_AGE_SECS)),
    )
}

fn effective_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn normalize_doc_url(base_url: &Url, value: &str) -> Result<Url, String> {
    let value = normalize_query(value)?;
    let url = match Url::parse(value) {
        Ok(url) => url,
        Err(_) => base_url
            .join(value.trim_start_matches('/'))
            .map_err(|error| format!("invalid document URL: {error}"))?,
    };

    validate_doc_url(base_url, &url)?;

    Ok(url)
}

fn validate_doc_url(base_url: &Url, candidate: &Url) -> Result<(), String> {
    if candidate.scheme() != "https" {
        return Err("document URL must use https".to_string());
    }

    let Some(host) = candidate.host_str() else {
        return Err("document URL must include a host".to_string());
    };

    let Some(base_host) = base_url.host_str() else {
        return Err("configured docs site URL is missing a host".to_string());
    };

    if host != base_host {
        return Err(format!("document URL host must be {base_host}"));
    }

    Ok(())
}

fn build_docs_url(base_url: &Url, path: &str) -> String {
    // `path` here comes from `normalize_search_result_path`, which always returns
    // a relative path beginning with `/docs/`. The `Url::parse` branch handles the
    // unlikely case where a caller passes an already-absolute URL.
    if let Ok(url) = Url::parse(path) {
        return url.to_string();
    }

    base_url
        .join(path.trim_start_matches('/'))
        .expect("default docs base URL should always be valid")
        .to_string()
}

fn resolve_doc_source_path(repo_root: &Path, doc_url: &Url) -> Result<PathBuf, String> {
    let doc_path = doc_url
        .path()
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();

    let route = doc_path
        .strip_prefix("docs/")
        .ok_or_else(|| format!("unsupported docs path: {}", doc_url.path()))?;

    let content_root = repo_root.join("src/content");

    for (collection_dir, route_prefix) in COLLECTION_ROUTES {
        if route == *route_prefix || route.starts_with(&format!("{route_prefix}/")) {
            let slug = route
                .strip_prefix(route_prefix)
                .unwrap_or("")
                .trim_start_matches('/');

            for candidate in source_candidates(&content_root.join(collection_dir), slug) {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }

    Err(format!(
        "could not map {} to a source file under {}",
        doc_url,
        content_root.display()
    ))
}

fn source_candidates(collection_root: &Path, slug: &str) -> Vec<PathBuf> {
    if slug.is_empty() {
        return vec![
            collection_root.join("index.mdx"),
            collection_root.join("index.md"),
        ];
    }

    vec![
        collection_root.join(format!("{slug}.mdx")),
        collection_root.join(format!("{slug}.md")),
        collection_root.join(slug).join("index.mdx"),
        collection_root.join(slug).join("index.md"),
    ]
}

fn extract_frontmatter_value(content: &str, key: &str) -> Option<String> {
    let frontmatter = frontmatter_block(content)?;
    let prefix = format!("{key}:");

    frontmatter
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(&prefix))
        .map(str::trim)
        // Block scalars (| > |- >- |+ >+) span multiple lines — skip them rather
        // than returning the bare indicator character as the value.
        .filter(|value| !matches!(*value, "|" | ">" | "|-" | ">-" | "|+" | ">+"))
        .map(trim_quotes)
        .filter(|value| !value.is_empty())
}

fn extract_heading_title(content: &str) -> Option<String> {
    content_without_frontmatter(content)
        .lines()
        .find_map(|line| line.trim().strip_prefix("# "))
        .map(str::trim)
        .map(ToOwned::to_owned)
        .filter(|value| !value.is_empty())
}

fn frontmatter_block(content: &str) -> Option<&str> {
    let content = content.strip_prefix("---\n")?;
    let end = content.find("\n---\n")?;
    Some(&content[..end])
}

fn content_without_frontmatter(content: &str) -> &str {
    if let Some(rest) = content.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            return &rest[end + 5..];
        }
    }

    content
}

fn trim_quotes(value: &str) -> String {
    if value.len() >= 2 {
        for delim in ['"', '\''] {
            if value.starts_with(delim) && value.ends_with(delim) {
                // Unescape any backslash-escaped occurrences of the delimiter
                // inside the string (e.g. `"Hello \"World\""` → `Hello "World"`).
                return value[1..value.len() - 1]
                    .replace(&format!("\\{delim}"), &delim.to_string());
            }
        }
    }
    value.to_string()
}

fn clone_docs_repo(repo_root: &Path) -> Result<(), String> {
    if let Some(parent) = repo_root.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("failed to create {}: {error}", parent.display())
        })?;
    }

    let git_url = env::var("SURREALDB_DOCS_REPO_GIT_URL")
        .unwrap_or_else(|_| DEFAULT_DOCS_REPO_GIT_URL.to_string());

    let output = Command::new("git")
        .args(["clone", "--depth", "1", "--filter=blob:none", "--sparse"])
        .arg(&git_url)
        .arg(repo_root)
        .output()
        .map_err(|error| format!("failed to run git clone: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["sparse-checkout", "set", "src/content"])
        .output()
        .map_err(|error| format!("failed to run git sparse-checkout: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "git sparse-checkout failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

fn refresh_docs_repo(repo_root: &Path) -> Result<(), String> {
    if !repo_root.exists() {
        return clone_docs_repo(repo_root);
    }

    let refresh_max_age = docs_repo_refresh_max_age()?;
    let fetch_head_path = repo_root.join(".git").join("FETCH_HEAD");

    if should_skip_docs_repo_refresh(&fetch_head_path, refresh_max_age)? {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("pull")
        .arg("--ff-only")
        .arg("--depth")
        .arg("1")
        .output()
        .map_err(|error| format!("failed to run git pull in {}: {error}", repo_root.display()))?;

    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "git pull failed in {}: {}",
        repo_root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn should_skip_docs_repo_refresh(
    fetch_head_path: &Path,
    refresh_max_age: Duration,
) -> Result<bool, String> {
    let metadata = match fs::metadata(fetch_head_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "failed to read {} metadata: {error}",
                fetch_head_path.display()
            ));
        }
    };
    let modified_at = metadata.modified().map_err(|error| {
        format!(
            "failed to read {} modified time: {error}",
            fetch_head_path.display()
        )
    })?;

    Ok(fetch_head_age(modified_at, SystemTime::now()) <= refresh_max_age)
}

fn fetch_head_age(modified_at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(modified_at).unwrap_or(Duration::ZERO)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let server = SurrealDocsServer::new().map_err(std::io::Error::other)?;

    let repo_path = server.docs_repo_path.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(
            Duration::from_secs(DOCS_REPO_SETUP_TIMEOUT_SECS),
            tokio::task::spawn_blocking(move || refresh_docs_repo(&repo_path)),
        )
        .await
        {
            Err(_elapsed) => eprintln!(
                "warning: docs repo setup timed out after {DOCS_REPO_SETUP_TIMEOUT_SECS}s"
            ),
            Ok(Err(join_error)) => eprintln!("warning: docs repo setup task panicked: {join_error}"),
            Ok(Ok(Err(error))) => eprintln!("warning: {error}"),
            Ok(Ok(Ok(()))) => {}
        }
    });

    server.serve(stdio()).await?.waiting().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn limit_defaults_and_caps() {
        assert_eq!(effective_limit(None), DEFAULT_LIMIT);
        assert_eq!(effective_limit(Some(0)), 1);
        assert_eq!(effective_limit(Some(3)), 3);
        assert_eq!(effective_limit(Some(200)), MAX_LIMIT);
    }

    #[test]
    fn docs_url_uses_absolute_or_relative_path() {
        let base = Url::parse(DEFAULT_DOCS_SITE_URL).unwrap();

        assert_eq!(
            build_docs_url(&base, "/docs/surrealdb/embedding"),
            "https://surrealdb.com/docs/surrealdb/embedding"
        );
        assert_eq!(
            build_docs_url(&base, "https://example.com/custom"),
            "https://example.com/custom"
        );
    }

    #[test]
    fn query_must_not_be_empty() {
        assert!(normalize_query("embedded").is_ok());
        assert!(normalize_query("   ").is_err());
    }

    #[test]
    fn fetch_head_is_fresh_within_default_refresh_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let modified_at = now - Duration::from_secs(DEFAULT_DOCS_REPO_REFRESH_MAX_AGE_SECS - 1);

        assert!(
            fetch_head_age(modified_at, now)
                < Duration::from_secs(DEFAULT_DOCS_REPO_REFRESH_MAX_AGE_SECS)
        );
    }

    #[test]
    fn fetch_head_is_stale_after_default_refresh_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let modified_at = now - Duration::from_secs(DEFAULT_DOCS_REPO_REFRESH_MAX_AGE_SECS + 1);

        assert!(
            fetch_head_age(modified_at, now)
                > Duration::from_secs(DEFAULT_DOCS_REPO_REFRESH_MAX_AGE_SECS)
        );
    }

    #[test]
    fn fetch_head_age_clamps_future_timestamps() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let modified_at = now + Duration::from_secs(60);

        assert_eq!(fetch_head_age(modified_at, now), Duration::ZERO);
    }

    #[test]
    fn linux_prefers_xdg_cache_home() {
        let path = default_docs_repo_path_for_test(
            "linux",
            Some(Path::new("/home/user")),
            Some(Path::new("/tmp/xdg")),
            None,
        )
        .unwrap();

        assert_eq!(
            path,
            PathBuf::from("/tmp/xdg/surrealdb-docs-mcp/docs.surrealdb.com")
        );
    }

    #[test]
    fn linux_defaults_to_dot_cache() {
        let path =
            default_docs_repo_path_for_test("linux", Some(Path::new("/home/user")), None, None)
                .unwrap();

        assert_eq!(
            path,
            PathBuf::from("/home/user/.cache/surrealdb-docs-mcp/docs.surrealdb.com")
        );
    }

    #[test]
    fn macos_defaults_to_library_caches() {
        let path =
            default_docs_repo_path_for_test("macos", Some(Path::new("/Users/user")), None, None)
                .unwrap();

        assert_eq!(
            path,
            PathBuf::from("/Users/user/Library/Caches/surrealdb-docs-mcp/docs.surrealdb.com")
        );
    }

    #[test]
    fn windows_defaults_to_local_app_data() {
        let path = default_docs_repo_path_for_test(
            "windows",
            Some(Path::new("/Users/user")),
            Some(Path::new("/ignored")),
            Some(Path::new("/AppData/Local")),
        )
        .unwrap();

        assert_eq!(
            path,
            PathBuf::from("/AppData/Local/surrealdb-docs-mcp/docs.surrealdb.com")
        );
    }

    #[test]
    fn wsl_uses_linux_xdg_cache_rules() {
        let path = default_docs_repo_path_for_test(
            "linux",
            Some(Path::new("/home/user")),
            Some(Path::new("/mnt/c/Users/user/.cache")),
            None,
        )
        .unwrap();

        assert_eq!(
            path,
            PathBuf::from("/mnt/c/Users/user/.cache/surrealdb-docs-mcp/docs.surrealdb.com")
        );
    }

    #[test]
    fn relative_xdg_cache_home_is_rejected() {
        let error = default_docs_repo_path_for_test(
            "linux",
            Some(Path::new("/home/user")),
            Some(Path::new("relative")),
            None,
        )
        .unwrap_err();

        assert_eq!(error, "XDG_CACHE_HOME must be an absolute path");
    }

    #[test]
    fn document_url_supports_relative_and_absolute_docs_urls() {
        let base = Url::parse(DEFAULT_DOCS_SITE_URL).unwrap();

        assert_eq!(
            normalize_doc_url(&base, "/docs/surrealdb/embedding")
                .unwrap()
                .as_str(),
            "https://surrealdb.com/docs/surrealdb/embedding"
        );
        assert_eq!(
            normalize_doc_url(&base, "https://surrealdb.com/docs/surrealdb/embedding")
                .unwrap()
                .as_str(),
            "https://surrealdb.com/docs/surrealdb/embedding"
        );
    }

    #[test]
    fn document_url_rejects_non_docs_hosts() {
        let base = Url::parse(DEFAULT_DOCS_SITE_URL).unwrap();

        assert!(normalize_doc_url(&base, "http://surrealdb.com/docs/surrealdb/embedding").is_err());
        assert!(normalize_doc_url(&base, "https://example.com/docs/surrealdb/embedding").is_err());
    }

    #[test]
    fn frontmatter_and_heading_are_extracted() {
        let content = r#"---
title: "Embedding SurrealDB"
description: Intro text
---

# Embedding SurrealDB

Body
"#;

        assert_eq!(
            extract_frontmatter_value(content, "title").as_deref(),
            Some("Embedding SurrealDB")
        );
        assert_eq!(
            extract_frontmatter_value(content, "description").as_deref(),
            Some("Intro text")
        );
        assert_eq!(
            extract_heading_title(content).as_deref(),
            Some("Embedding SurrealDB")
        );
    }

    #[test]
    fn frontmatter_handles_edge_cases() {
        // Escaped inner quotes are unescaped.
        let escaped = "---\ntitle: \"Hello \\\"World\\\"\"\n---\n";
        assert_eq!(
            extract_frontmatter_value(escaped, "title").as_deref(),
            Some("Hello \"World\"")
        );

        // Mismatched outer quotes are left as-is.
        let mismatched = "---\ntitle: \"oops'\n---\n";
        assert_eq!(
            extract_frontmatter_value(mismatched, "title").as_deref(),
            Some("\"oops'")
        );

        // Block scalars return None rather than the bare indicator character.
        let block = "---\ndescription: |\n  multi\n  line\n---\n";
        assert_eq!(extract_frontmatter_value(block, "description"), None);
    }

    // Requires the docs repo to be present at the default cache location or under
    // vendor/docs.surrealdb.com. Run with `cargo test -- --include-ignored` after
    // bootstrapping the repo with scripts/bootstrap-docs-repo.sh.
    #[test]
    #[ignore]
    fn source_path_is_resolved_from_cached_repo() {
        let repo_root = docs_repo_fixture_root();
        let url = Url::parse("https://surrealdb.com/docs/surrealdb/embedding").unwrap();
        let path = resolve_doc_source_path(&repo_root, &url).unwrap();

        assert!(path.ends_with("src/content/doc-surrealdb/embedding/index.mdx"));
        assert!(path.is_file());
    }

    // Requires the docs repo — see source_path_is_resolved_from_cached_repo above.
    #[test]
    #[ignore]
    fn sdk_route_maps_to_sdk_collection() {
        let repo_root = docs_repo_fixture_root();
        let url = Url::parse("https://surrealdb.com/docs/sdk/golang/start").unwrap();
        let path = resolve_doc_source_path(&repo_root, &url).unwrap();

        assert!(path.ends_with("src/content/doc-sdk-golang/start.mdx"));
        assert!(path.is_file());
    }

    #[test]
    fn search_hostname_matches_docs_site_mapping() {
        assert_eq!(
            docs_search_hostname("surrealdb.com"),
            "main--surrealdb-docs.netlify.app"
        );
        assert_eq!(
            docs_search_hostname("docs.surrealdb.com"),
            "main--surrealdb-docs.netlify.app"
        );
        assert_eq!(
            docs_search_hostname("preview.surrealdb.com"),
            "preview.surrealdb.com"
        );
    }

    #[test]
    fn api_result_maps_relative_docs_path() {
        let base = Url::parse(DEFAULT_DOCS_SITE_URL).unwrap();
        let result = map_search_result(
            &base,
            DocsSearchApiResult {
                title: "Embedding SurrealDB".to_string(),
                description: "Docs".to_string(),
                hostname: "main--surrealdb-docs.netlify.app".to_string(),
                score: 201.0,
                url: "/docs/surrealdb/embedding".to_string(),
            },
        )
        .unwrap();

        assert_eq!(result.path, "/docs/surrealdb/embedding");
        assert_eq!(result.url, "https://surrealdb.com/docs/surrealdb/embedding");
    }

    #[test]
    fn api_result_rejects_non_docs_path() {
        let error = normalize_search_result_path("/blog/launch").unwrap_err();
        assert!(error.contains("not a docs path"));
    }

    #[test]
    fn refresh_max_age_defaults_to_six_hours() {
        let _guard = test_env_lock().lock().unwrap();
        unsafe {
            env::remove_var("SURREALDB_DOCS_REPO_REFRESH_MAX_AGE_SECS");
        }

        assert_eq!(
            docs_repo_refresh_max_age().unwrap(),
            Duration::from_secs(DEFAULT_DOCS_REPO_REFRESH_MAX_AGE_SECS)
        );
    }

    #[test]
    fn refresh_max_age_honors_env_override() {
        let _guard = test_env_lock().lock().unwrap();
        unsafe {
            env::set_var("SURREALDB_DOCS_REPO_REFRESH_MAX_AGE_SECS", "60");
        }

        assert_eq!(
            docs_repo_refresh_max_age().unwrap(),
            Duration::from_secs(60)
        );

        unsafe {
            env::remove_var("SURREALDB_DOCS_REPO_REFRESH_MAX_AGE_SECS");
        }
    }

    fn default_docs_repo_path_for_test(
        target_os: &str,
        home_dir: Option<&Path>,
        xdg_cache_home: Option<&Path>,
        local_app_data: Option<&Path>,
    ) -> Result<PathBuf, String> {
        Ok(
            default_cache_home_for(target_os, home_dir, xdg_cache_home, local_app_data)?
                .join(DEFAULT_APP_DIR_NAME)
                .join(DEFAULT_DOCS_REPO_DIR_NAME),
        )
    }

    fn docs_repo_fixture_root() -> PathBuf {
        let legacy_vendor_path = PathBuf::from("vendor/docs.surrealdb.com");
        if legacy_vendor_path.exists() {
            return legacy_vendor_path;
        }

        default_docs_repo_path()
            .expect("default docs repo path should resolve for test fixture lookup")
    }

    fn test_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
