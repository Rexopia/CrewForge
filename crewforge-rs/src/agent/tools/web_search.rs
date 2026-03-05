use crate::agent::ToolResult;
use async_trait::async_trait;
use reqwest::Client;

/// Web search tool using Brave Search API.
///
/// Requires `BRAVE_API_KEY` environment variable.
/// Free tier: 2000 queries/month at <https://brave.com/search/api/>.
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
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    fn api_key(&self) -> Option<String> {
        std::env::var("BRAVE_API_KEY").ok().filter(|k| !k.is_empty())
    }
}

#[async_trait]
impl crate::agent::Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using Brave Search API. Requires BRAVE_API_KEY environment variable. \
         Free tier: 2000 queries/month."
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
        let Some(api_key) = self.api_key() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(
                    "web_search is unavailable: BRAVE_API_KEY not configured. \
                     Get a free API key at https://brave.com/search/api/ and set it as \
                     the BRAVE_API_KEY environment variable."
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

        match brave_search(&self.client, &api_key, query, max_results).await {
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

/// Call Brave Search API.
async fn brave_search(
    client: &Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .query(&[("q", query), ("count", &max_results.to_string())])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Brave API returned {status}: {body}");
    }

    let json: serde_json::Value = resp.json().await?;

    let results = json
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .filter_map(|item| {
                    let title = item.get("title")?.as_str()?.to_string();
                    let url = item.get("url")?.as_str()?.to_string();
                    let snippet = item
                        .get("description")
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
    async fn no_api_key_returns_error() {
        // Ensure no key is set for this test.
        let tool = WebSearchTool::new();
        if tool.api_key().is_some() {
            return; // Skip if key is actually configured.
        }

        let result = tool
            .execute(serde_json::json!({"query": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("BRAVE_API_KEY"));
    }

    #[test]
    fn parse_brave_response() {
        let json: serde_json::Value = serde_json::json!({
            "web": {
                "results": [
                    {
                        "title": "Rust Language",
                        "url": "https://rust-lang.org",
                        "description": "A systems programming language"
                    },
                    {
                        "title": "Rust Docs",
                        "url": "https://doc.rust-lang.org",
                        "description": "Official documentation"
                    }
                ]
            }
        });

        let results: Vec<SearchResult> = json
            .get("web")
            .unwrap()
            .get("results")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| {
                Some(SearchResult {
                    title: item.get("title")?.as_str()?.to_string(),
                    url: item.get("url")?.as_str()?.to_string(),
                    snippet: item.get("description")?.as_str()?.to_string(),
                })
            })
            .collect();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Language");
        assert_eq!(results[0].url, "https://rust-lang.org");
    }
}
