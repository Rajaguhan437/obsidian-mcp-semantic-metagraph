# Tool reference

19 tools. Parameter names below are the actual schema names — several differ from
what you might guess, so they are listed explicitly.

## Search

### `search_semantic`
Meaning-based retrieval over chunk and summary vectors.

| param | required | notes |
|---|---|---|
| `query` | yes | |
| `top_k` | no | default 10 |
| `lexical_prefetch` | no | default false. **Leave false** — true enables the legacy BM25-gated re-rank, which caps recall at the top 50 lexical hits |
| `alpha` | no | BM25 weight in the legacy path only |
| `include_content` | no | return matched content |

Requires `OBSIDIAN_EMBEDDINGS=true` and a built index. While the index is warming
it returns an explicit "not ready" error rather than silently degrading.

**Result fields** (beyond `path`, `title`, `score`, `tags`, `snippet`, `content`):

| field | notes |
|---|---|
| `match_type` | `chunk` \| `summary` \| `note` — which representation produced `score` |
| `best_chunk` | the note's most relevant passage: `index`, `heading_path` (array, outermost first), `passage`, `score` (raw cosine) |
| `summary_score` | the summary arm's weighted score, as a number |

`best_chunk` is present on every hit that has chunks, **including when
`match_type` is `summary`**. In that case the passage is the note's most relevant
one but did *not* cause the ranking — branch on `match_type`, not on the
presence of `best_chunk`.

`score` is a ranking score, not a cosine: a weighted summary win can exceed 1.0.

Both fields are absent in `daemon` mode (note-level IPC protocol) and when
`lexical_prefetch` is true (a blended rank is not attributable to one arm).

### `search_text`
Tantivy BM25 across title, headings, tags, frontmatter and full body, with field
boosts. Use this for exact strings, identifiers and terminology.

| param | required |
|---|---|
| `query` | yes |
| `context_len`, `fields`, `limit` | no |

### `search_regex`
Regex across note bodies. `pattern` (required). Pattern length and compiled size
are bounded.

### `search_metadata`
Structured queries over tags and frontmatter.

| param | required | notes |
|---|---|---|
| `type` | yes | e.g. `"tag"` |
| `tag` | when `type` is `tag` | |
| `field`, `value`, `operator`, `include_nested` | no | |

## Graph

### `wikilinks`
| param | required | notes |
|---|---|---|
| `query` | yes | `backlinks` \| `outgoing` \| `broken` \| `orphans` |
| `path` | for `backlinks`/`outgoing` | optional for `broken`, unused for `orphans` |

- **backlinks** — sources linking to a note, with each link's raw text and line
- **outgoing** — links from a note, each with `resolved_path` (null if broken),
  `heading`, `block_ref`, `alias`
- **broken** — links whose target does not resolve
- **orphans** — disconnected notes, with `status` separating "no links at all"
  from "only broken links"

Results are **not deterministically ordered**; compare order-insensitively.

## Read

| tool | required params | notes |
|---|---|---|
| `note_read` | `path` | full markdown |
| `note_read_many` | `paths` | byte-capped |
| `note_inspect` | `path` | headings, tags, block refs, backlink count |
| `frontmatter` | `action`, `path` | `action`: `get` \| `set` \| … ; `key`, `value` for writes |

## Write

| tool | required params | notes |
|---|---|---|
| `note_create` | `path`, `content` | |
| `note_write` | `path`, `content` | overwrites |
| `note_insert` | `path`, `content` | |
| `note_patch` | `path`, `operation`, `target_type`, `target`, `content` | `target_type` e.g. `heading`; nested headings use `::` |
| `note_move` | `from`, `to` | **not** `from_path`/`to_path` |
| `note_delete` | `path`, `confirm` | `confirm` must be true |

Writes update the vault index, lexical index and link graph incrementally, and
queue the note for re-embedding.

## Navigate

| tool | required params | notes |
|---|---|---|
| `vault_list` | — | all note paths |
| `vault_info` | — | counts, tags, index status |
| `periodic` | `action`, `period` | `action`: `get` \| `create` \| `list`; `period`: `daily` \| … |
| `open_in_obsidian` | `path` | opens via `obsidian://` URI |

## Restricting the surface

`OBSIDIAN_TOOLS` accepts a profile (`full`, `core`, `read`, `minimal`), a
comma-separated allow-list, or a `!`-prefixed deny-list. Useful for giving an
agent read-only access:

```bash
OBSIDIAN_TOOLS=read
OBSIDIAN_TOOLS='!note_delete,!note_write,!note_patch'
```
