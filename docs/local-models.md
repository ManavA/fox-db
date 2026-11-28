# fox-db with local models

In the C caller benchmark, Inkling-2-Mini improved from a 0.42 pass rate with grep
to 0.75 with the `fox` CLI and 0.83 with a fox packet. Qwen3-8B Q4 scored 0 on all
three arms across 6 C caller tasks. Haiku showed no meaningful accuracy gain
(F1 +0.02). See the [benchmark results](benchmarks.md#local-models).

Use a model that can already make tool calls reliably.

For local models, pass a larger packet: `fox packet "<question>" --budget 3200`. On the
oracle caller tasks a local model passed 59 of 224 runs at 3200 tokens against 37 at the
800-token default, for 9% more tokens. Haiku showed no difference (27 and 26 of 66), so
the default stays at 800.

Every config below starts the same stdio MCP server: `<home>/.foxdb/bin/fox mcp`. Use the
absolute path to `fox`; write your own home in place of `/path/to/home`. Index each project first
with `fox index .`.

Config formats were checked against each tool's docs in November 2026. These tools change their
config shape often; if a field is rejected, compare with the linked page.

## Serve the model with enough context

Tool definitions, the system prompt and a few tool results fill a small context window fast. Give
the model at least 32k tokens; 64k is safer.

### Ollama

Ollama's default context is too small for agent tool use. Raise it for the whole server:

```sh
OLLAMA_CONTEXT_LENGTH=65536 ollama serve
```

or per request with the `num_ctx` option (`/set parameter num_ctx 65536` inside `ollama run`).
Ollama's docs: <https://docs.ollama.com/context-length> and <https://docs.ollama.com/faq>.

### llama.cpp

`llama-server` needs its Jinja template engine to apply the model's chat template, which is what
makes tool calls work through its OpenAI-compatible endpoint. Current builds enable it by default;
pass `--jinja` anyway so older builds behave the same:

```sh
llama-server -m model.gguf --jinja -c 65536 --port 8080
```

The endpoint is then `http://127.0.0.1:8080/v1`. Any of the tools below that accepts an
OpenAI-compatible provider can use it. Docs:
<https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md> and
<https://github.com/ggml-org/llama.cpp/blob/master/docs/function-calling.md>.

## OpenCode with Ollama

OpenCode finds a local Ollama at `http://127.0.0.1:11434` on its own and lists its models under
`/models`. Set `baseURL` only for a remote Ollama. MCP servers go under `mcp.servers`. In
`opencode.jsonc`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "providers": {
    "ollama": { "settings": { "baseURL": "http://127.0.0.1:11434/v1" } }
  },
  "mcp": {
    "servers": {
      "foxdb": { "type": "local", "command": ["/path/to/home/.foxdb/bin/fox", "mcp"] }
    }
  }
}
```

Or from the shell: `opencode mcp add foxdb -- /path/to/home/.foxdb/bin/fox mcp`. Older OpenCode
releases put servers directly under `mcp` (the form in [AGENTS-SETUP.md](../AGENTS-SETUP.md)).
Docs: <https://opencode.ai/docs/mcp-servers/> and <https://opencode.ai/docs/providers/>.

## Goose

In `~/.config/goose/config.yaml`:

```yaml
extensions:
  foxdb:
    type: stdio
    name: foxdb
    enabled: true
    cmd: /path/to/home/.foxdb/bin/fox
    args: ["mcp"]
    timeout: 300
```

`goose configure` (Add Extension, Command-line Extension) writes the same entry. Pick Ollama as
the provider in `goose configure`. Docs: <https://goose-docs.ai/docs/guides/config-files/> and
<https://goose-docs.ai/docs/getting-started/using-extensions/>.

## Cline

Open the MCP Servers panel, choose Configure, and add to `cline_mcp_settings.json`:

```json
{
  "mcpServers": {
    "foxdb": {
      "command": "/path/to/home/.foxdb/bin/fox",
      "args": ["mcp"],
      "disabled": false,
      "autoApprove": []
    }
  }
}
```

Docs: <https://docs.cline.bot/mcp/configuring-mcp-servers>.

## Continue

Continue uses MCP only in agent mode. Save this as `.continue/mcpServers/foxdb.yaml` in the
workspace:

```yaml
name: foxdb
version: 0.0.1
schema: v1
mcpServers:
  - name: foxdb
    type: stdio
    command: /path/to/home/.foxdb/bin/fox
    args:
      - mcp
```

Continue also reads a Cline-style JSON file placed in `.continue/mcpServers/`. Docs:
<https://docs.continue.dev/customize/deep-dives/mcp>.

## Without MCP

If your model can run shell commands reliably, put `fox` on PATH and tell it to use
`fox find WORDS`, `fox code NAME --rel callers` and `fox code NAME --rel impact`; the benchmark's `fox-cli` arm
works this way.
