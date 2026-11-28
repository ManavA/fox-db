# Listing fox-db in MCP registries

fox-db runs locally over stdio. The listing name is **fox-db**. Launch the server with `fox mcp` after installing `fox` and `foxd`. Use `https://github.com/ManavA/fox-db` as the source URL. A short description for directory forms is “Search code and docs, trace call graphs, and query project memory from a local index.”

fox-db's [GitHub releases](https://github.com/ManavA/fox-db/releases) distribute per-platform `foxdb-<version>-<target>.tar.gz` files and SHA-256 checksums. The installer places the executables in `~/.foxdb/bin` by default. Use an absolute executable path in client configurations when that directory is outside the client's PATH.

To install and connect locally on Linux or macOS, run the following from a terminal. Run the index command from the Git checkout you want to search.

```sh
curl -fsSL https://raw.githubusercontent.com/ManavA/fox-db/main/scripts/install.sh | sh
"$HOME/.foxdb/bin/fox" index .
```

If you already downloaded a release archive, keep its matching `.sha256` file alongside it and run `bash scripts/install.sh --tarball /path/to/foxdb-<version>-<target>.tar.gz` from a fox-db checkout. Replace the placeholder path with the downloaded filename.

For Claude Code or Codex, run the command for your client, then open a new session in the indexed checkout.

```sh
claude mcp add --scope user foxdb -- "$HOME/.foxdb/bin/fox" mcp
codex mcp add foxdb -- "$HOME/.foxdb/bin/fox" mcp
```

See the [manual setup](../README.md#manual-install-5-minutes) for the required tools and the source-build fallback.

## Official MCP Registry

The root [server.json](../server.json) uses the Registry's documented manual-install path. Its `websiteUrl` points to this guide so users can install via the existing public release. Registry clients need package metadata to automate installation. [Registry server.json guide](https://github.com/modelcontextprotocol/registry/blob/main/docs/reference/server-json/generic-server-json.md#server-with-custom-installation-path).

1. Check that this guide is live at the public `websiteUrl` in `server.json`. Install the [current MCP publisher](https://github.com/modelcontextprotocol/registry/releases/latest).
2. From the directory containing `server.json`, run `mcp-publisher validate` and `mcp-publisher login github`. Follow the printed GitHub device-code link and authorize with the `ManavA` account. Confirm the authorized namespace matches the exact casing of `io.github.ManavA/fox-db`. [Publisher CLI reference](https://github.com/modelcontextprotocol/registry/blob/main/docs/reference/cli/commands.md), [publishing quickstart](https://github.com/modelcontextprotocol/registry/blob/main/docs/modelcontextprotocol-io/quickstart.mdx).
3. Run `mcp-publisher publish`, then look for the version declared in `server.json` in the [public Registry](https://registry.modelcontextprotocol.io/?q=io.github.ManavA%2Ffox-db).

For automated installation in a future release, publish an actual MCP Bundle (`.mcpb`) containing both executables and a manifest that starts `fox mcp`. GitHub or GitLab release asset URLs for MCPB must contain `mcp`, and each `packages` entry needs the asset's SHA-256 in `fileSha256`. The existing tarballs are a different format. A Cargo alternative needs an installable public crates.io package that installs both binaries, with `mcp-name: io.github.ManavA/fox-db` in visible README text, stdio transport, and `mcp` as a positional package argument. Cargo users need a Rust toolchain. [Supported package types](https://github.com/modelcontextprotocol/registry/blob/main/docs/modelcontextprotocol-io/package-types.mdx), [MCP Bundle format](https://github.com/anthropics/mcpb).

## Smithery

Smithery retired its accounts, CLI, API, and connection management on October 31, 2026. The public catalog remains a curated directory powered by Arcade. The current directory and transition announcement provide no self-service submission procedure for fox-db's local stdio server. Older Smithery CLI or hosted publishing instructions predate the shutdown. [Current Smithery directory](https://www.smithery.ai/), [Smithery transition announcement](https://www.smithery.ai/future).

## MCP.so

1. Open the [MCP.so submission form](https://mcp.so/submit?type=server) and choose **MCP Server**. Supply `https://github.com/ManavA/fox-db` in **Repository URL** and **fox-db** in **Name**. The form uses the public repository for this project type.
2. Choose **Pay and submit automatically** and complete checkout. The form lists a one-time USD 39 fee and says the listing goes live immediately after payment. Verify the resulting page links to the correct repository and shows the local setup. The form links to **submit a ticket** for questions. [MCP.so submission form](https://mcp.so/submit?type=server).

## Glama

1. Open the [Glama MCP directory](https://glama.ai/mcp/servers), choose **Add MCP Server**, and enter the public GitHub repository URL, the **fox-db** display name, and a short description. This is the open-source repository listing flow. Glama's automatic import from the official Registry covers Streamable HTTP connectors. [Glama submission FAQ](https://glama.ai/mcp/faq#how-do-i-submit-an-mcp-server).
2. Open the listing after indexing and choose **Claim ownership**. Sign in with GitHub as `ManavA`. Glama supports direct GitHub ownership claims for personal repositories. A root `glama.json` can name maintainers for organization repositories or delegated claims. [Glama ownership instructions](https://glama.ai/blog/2025-07-08-what-is-glamajson).
3. Check automated license, scan, and health results. Adjust the description and README under **Admin → Listing**. If Glama's build or health check needs a custom build, use **Admin → Dockerfile** to build or install both binaries and start `fox mcp`. Glama's build environment has its own filesystem, so any functional check that needs indexed source should use a fixture there. Users running fox-db locally index their own checkout. [Glama submission FAQ](https://glama.ai/mcp/faq#how-do-i-submit-an-mcp-server), [fox-db setup](../README.md#manual-install-5-minutes).
