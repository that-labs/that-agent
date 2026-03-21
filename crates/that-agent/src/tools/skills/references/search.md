# Search Tools

Full flag reference for `that search` — query (web search) and fetch (URL reader), all engines and modes.

## Quick start

```bash
# Search the web
that search query "pydantic v2 migration guide" --limit 5 --max-tokens 1200

# Fetch a URL and extract content
that search fetch https://docs.example.com/guide --mode markdown --max-tokens 3000

# Batch fetch multiple URLs in one call
that search fetch https://example.com/a https://example.com/b --mode scrape
```

---

## query — find pages

```bash
that search query "<query>" [--engine ENGINE] [--limit N] [--no-cache] [--max-tokens N]
```

**Free engines (no API key):**

| Engine | Notes |
|--------|-------|
| `duckduckgo` | Default. Tries lite endpoint first. |
| `bing` | Independent index. Good coverage. |
| `yahoo` | Bing index. Extra fallback. |
| `mojeek` | Independent crawler. Most bot-friendly. |

**Premium (API key required):** `tavily` (`TAVILY_API_KEY`), `brave` (`BRAVE_API_KEY`).

that-tools auto-falls back through the chain if the primary engine fails.

Output: `results[]` (title, url, snippet, score), `engine`, `cached`.

---

## fetch — read pages

```bash
that search fetch <url> [<url2> ...] [--mode MODE] [--max-tokens N]
```

Multiple URLs fetch in parallel — batch them rather than calling separately.

### Modes

| Mode | When to use |
|------|-------------|
| `scrape` | Default. Auto-runs Python/BeautifulSoup extractor → `scraped_content[]`. Start here. |
| `inspect` | DOM structure analysis — use when scrape returns empty. Returns `content_regions`, `repeating_patterns`. |
| `markdown` | HTML → readable markdown. Good for documentation pages. |
| `text` | Plain text, all markup stripped. Fastest for simple content. |

**If `scraped_content` is empty:** switch to `--mode inspect`, read `content_regions` and `repeating_patterns`, then write a custom extractor with `that exec run "uv run --with beautifulsoup4 python3 - <<'PY' ..."`.

---

## Workflow

```bash
# 1. Find pages
that search query "tokio async tutorial" --limit 5 --max-tokens 1200

# 2. Fetch the best URLs (batch)
that search fetch https://tokio.rs/tokio/tutorial https://docs.rs/tokio \
  --mode scrape --max-tokens 3000

# 3. If scraped_content is empty, inspect then extract manually
that search fetch https://example.com --mode inspect --max-tokens 1500
```

Always set `--max-tokens` — pages can be large. Check `that mem recall "topic"` before hitting the web.
