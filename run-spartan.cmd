@echo off
rem ---------------------------------------------------------------------------
rem  Start the retrieval bridge for Spartan.
rem
rem    run-spartan.cmd          foreground - the log is visible here (embedding
rem                             progress, scope counts, model errors). Closing
rem                             the window stops the server.
rem    run-spartan.cmd serve    detached - returns immediately and the server
rem                             outlives this window. Use this one when Hermes
rem                             needs it up all day. Log goes to
rem                             %LOCALAPPDATA%\obsidian-mcp\obsidian-mcp.log
rem    run-spartan.cmd stop     stop whatever is on the port.
rem
rem  Hermes reaches it at http://127.0.0.1:37842/mcp, which is what
rem  %LOCALAPPDATA%\hermes\config.yaml already points at.
rem ---------------------------------------------------------------------------

set "OBSIDIAN_VAULT_PATH=D:\Personal\Obsidian Vault"

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
rem  "passage: ") are the pair this vault's retrieval was benchmarked with.
rem  Leave them alone.

rem  In-process embeddings. No separate daemon: one vault, one client, and the
rem  daemon does not read .obsidian-mcp/ignore, so keeping it out of the picture
rem  removes a whole class of "why is that note in my results" questions.
set "OBSIDIAN_SEMANTIC_MODE=local"

rem -- retrieval scopes -------------------------------------------------------
rem  Defined in .obsidian-mcp/scopes: capt_sparroz, spartan, combined.
rem  The default keeps unscoped searches on his knowledge, so indexing the
rem  agent's space did not quietly change what an ordinary search returns.
rem  A query can still ask for "spartan", "combined" or "all" explicitly.
set "OBSIDIAN_DEFAULT_SCOPE=capt_sparroz"

rem -- run ---------------------------------------------------------------------
rem  `serve` daemonises: it stops anything already on the port, spawns a
rem  detached child and exits. That matters when the server is started from
rem  another program's console - a foreground server dies with whatever
rem  launched it, and Hermes then finds nothing on 37842.
if /I "%~1"=="serve" (
  "%~dp0target\release\obsidian-mcp.exe" serve
) else if /I "%~1"=="stop" (
  "%~dp0target\release\obsidian-mcp.exe" stop
) else (
  "%~dp0target\release\obsidian-mcp.exe" --http
)
