use crate::agent::ToolResult;
use async_trait::async_trait;
use reqwest::Client;

/// Web search tool using a SearXNG instance.
///
/// Requires `SEARXNG_URL` environment variable (e.g. `http://localhost:8080`).
/// SearXNG is a free, self-hosted metasearch engine that aggregates results
/// from multiple search engines (Google, Bing, DuckDuckGo, Brave, etc.).
///
/// Quick start: `docker run -d -p 8080:8080 searxng/searxng`
/// Docs: <https://docs.searxng.org>
pub struct WebSearchTool {
    client: Client,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .no_proxy()
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    fn base_url(&self) -> Option<String> {
        std::env::var("SEARXNG_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string())
    }
}

#[async_trait]
impl crate::agent::Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using a SearXNG instance. Requires SEARXNG_URL environment variable \
         (e.g. http://localhost:8080). Aggregates results from multiple search engines."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (1-10, default: 5)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(base_url) = self.base_url() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "web_search is unavailable: SEARXNG_URL not configured. \
                     Set the SEARXNG_URL environment variable to your SearXNG instance \
                     (e.g. http://localhost:8080). \
                     Quick start: docker run -d -p 8080:8080 searxng/searxng"
                        .into(),
                ),
            });
        };

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'query' parameter"))?;

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| v.clamp(1, 10) as usize)
            .unwrap_or(5);

        match searxng_search(&self.client, &base_url, query, max_results).await {
            Ok(results) if results.is_empty() => {
                Ok(ToolResult::ok("No search results found."))
            }
            Ok(results) => {
                let mut output = String::new();
                for (i, result) in results.iter().enumerate() {
                    output.push_str(&format!(
                        "{}. {}\n   URL: {}\n   {}\n\n",
                        i + 1,
                        result.title,
                        result.url,
                        result.snippet
                    ));
                }
                Ok(ToolResult::ok(output.trim_end()))
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Search failed: {e}")),
            }),
        }
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Call SearXNG JSON API.
async fn searxng_search(
    client: &Client,
    base_url: &str,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let url = format!("{base_url}/search");
    let resp = client
        .get(&url)
        .query(&[
            ("q", query),
            ("format", "json"),
            ("pageno", "1"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 403 {
            anyhow::bail!(
                "SearXNG returned 403 Forbidden. \
                 Ensure JSON format is enabled in SearXNG settings \
                 (search.formats must include 'json')."
            );
        }
        anyhow::bail!("SearXNG returned {status}: {body}");
    }

    let json: serde_json::Value = resp.json().await?;

    let results = json
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .filter_map(|item| {
                    let title = item.get("title")?.as_str()?.to_string();
                    let url = item.get("url")?.as_str()?.to_string();
                    let snippet = item
                        .get("content")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(SearchResult {
                        title,
                        url,
                        snippet,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Tool;

    #[test]
    fn tool_metadata() {
        let tool = WebSearchTool::new();
        assert_eq!(crate::agent::Tool::name(&tool), "web_search");
        assert!(!crate::agent::Tool::is_mutating(&tool));
    }

    #[tokio::test]
    async fn no_url_returns_error() {
        let tool = WebSearchTool::new();
        if tool.base_url().is_some() {
            return; // Skip if URL is actually configured.
        }

        let result = tool
            .execute(serde_json::json!({"query": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("SEARXNG_URL"));
    }

    #[test]
    fn parse_searxng_response() {
        let json: serde_json::Value = serde_json::json!({
            "query": "rust",
            "results": [
                {
                    "title": "Rust Language",
                    "url": "https://rust-lang.org",
                    "content": "A systems programming language",
                    "engines": ["google", "duckduckgo"],
                    "score": 8.0
                },
                {
                    "title": "Rust Docs",
                    "url": "https://doc.rust-lang.org",
                    "content": "Official documentation",
                    "engines": ["google"],
                    "score": 4.0
                }
            ]
        });

        let results: Vec<SearchResult> = json
            .get("results")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| {
                Some(SearchResult {
                    title: item.get("title")?.as_str()?.to_string(),
                    url: item.get("url")?.as_str()?.to_string(),
                    snippet: item.get("content")?.as_str()?.to_string(),
                })
            })
            .collect();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Language");
        assert_eq!(results[0].url, "https://rust-lang.org");
    }
}
