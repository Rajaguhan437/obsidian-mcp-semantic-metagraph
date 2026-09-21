@echo off
rem ---------------------------------------------------------------------------
rem  Example launcher: one vault, several named retrieval scopes, embeddings
rem  served by a local Ollama over its OpenAI-compatible endpoint.
rem
rem  Copy this to run-spartan.cmd (gitignored) and set the vault path below.
rem
rem  Runs in the FOREGROUND so the log is visible: embedding progress, scope
rem  counts and any model error all appear here. Close the window to stop it.
rem ---------------------------------------------------------------------------

set "OBSIDIAN_VAULT_PATH=C:\path\to\your\vault"

rem -- transport --------------------------------------------------------------
set "OBSIDIAN_TRANSPORT=http"
set "OBSIDIAN_HTTP_HOST=127.0.0.1"
set "OBSIDIAN_HTTP_PORT=37842"
set "OBSIDIAN_WATCH=true"
set "OBSIDIAN_LOG_LEVEL=info"

rem -- lexical layer ----------------------------------------------------------
set "OBSIDIAN_TANTIVY=true"

rem -- semantic layer ---------------------------------------------------------
rem  Arctic Embed v2 served by Ollama's OpenAI-compatible endpoint. 1024-dim;
rem  stating the dimension skips a probe request on every cold start.
set "OBSIDIAN_EMBEDDINGS=true"
set "OBSIDIAN_EMBEDDING_PROVIDER=api"
set "OBSIDIAN_EMBEDDINGS_MODEL=snowflake-arctic-embed2"
set "OBSIDIAN_EMBEDDING_API_BASE=http://localhost:11434/v1"
set "OBSIDIAN_EMBEDDING_API_MODEL=snowflake-arctic-embed2:latest"
rem  Ollama ignores the key; the client requires the variable to be set.
set "OBSIDIAN_EMBEDDING_API_KEY=ollama"
set "OBSIDIAN_EMBEDDING_DIM=1024"

rem  The query/document prefixes are deliberately NOT set here. Arctic is an
rem  asymmetric model: queries and documents only land in the same space when
rem  each carries its own prefix, and the built-in defaults ("query: " and
rem  "passage: ") are the pair this fork's retrieval was benchmarked with.
rem  Leave them alone unless you switch to a model that wants a different pair.

rem  In-process embeddings. No separate daemon: one vault, one client, and the
rem  daemon does not read .obsidian-mcp/ignore, so keeping it out of the picture
rem  removes a whole class of "why is that note in my results" questions.
set "OBSIDIAN_SEMANTIC_MODE=local"

rem -- retrieval scopes -------------------------------------------------------
rem  Scope names come from .obsidian-mcp/scopes; `vault_info` and /dashboard
rem  list them with a live note count each.
rem
rem  Setting a default matters whenever you widen the index: a folder can join
rem  the index as its own scope while unscoped queries keep returning exactly
rem  what they returned before. A query reaches the rest by naming a scope, or
rem  "all".
set "OBSIDIAN_DEFAULT_SCOPE=your_default_scope"

"%~dp0target\release\obsidian-mcp.exe" --http
