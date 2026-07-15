# Lucia

English | [简体中文](README.zh-CN.md)

**An inspectable, configurable, and extensible terminal agent.**

Lucia is a fully open-source agent runtime and terminal interface for working with language models, tools, sessions, and WASM plugins. It is designed for developers who want to inspect the complete request path, keep capabilities optional, and replace concrete features without changing the Agent Core.

[Quick start](#quick-start) · [Build and install](#build-and-install) · [Configuration](#configure-a-model) · [Documentation](#documentation) · [Security](#security-model)

> [!NOTE]
> Lucia is in early development. APIs and the plugin protocol may change before a stable release.

## Why Lucia

- **Inspectable by design.** The model request path, Agent loop, tool routing, session storage, plugin loading, and permission checks are available under the MIT License.
- **Capabilities stay optional.** MCP, Skills, commands, context compression, planning, approvals, and multi-agent collaboration are independent plugins rather than mandatory Core behavior.
- **Permissions are explicit.** Plugins declare capabilities in their manifests, while the Host controls trusted identity, ownership, and resource limits.
- **Composable beyond the TUI.** Lucia can run as a small Agent Core, a plugin-enabled terminal application, or a set of Rust libraries embedded in another program.

## Capabilities

- Work with real models from a terminal while saving and resuming sessions per project.
- Connect to OpenAI, Anthropic, and OpenAI-compatible model services.
- Add MCP, Skills, commands, context compression, planning, approvals, and multi-agent collaboration through WASM plugins.
- Run as a lightweight Agent Core or compose session storage, Runtime, Plugin Host, and the TUI as needed.
- Embed Lucia as a Rust library instead of using the bundled terminal interface.

Lucia currently ships official Context, MCP, Skill, Command, Teammate, Plan, and Sandbox plugins. Plugins are not an afterthought; they are the primary extension model. Core owns the general agent mechanics, while each concrete capability stays in its own plugin.

## Quick Start

To verify the project with only Rust installed, run this from the repository root:

```bash
cargo run -p agent-basic-cli -- --demo "hello"
```

This example uses a deterministic built-in model. It needs no API key and does not contact an external model service. It exercises one complete model request, native tool call, and result round trip.

## Requirements

- Rust stable. The exact toolchain is defined in `rust-toolchain.toml`.
- `wasm32-wasip2`, only when building WASM plugins.
- Bun is optional. It automates bulk builds and packaging for the official plugins, but it is not required to build Lucia Core.

Install the WASM target manually if it is missing:

```bash
rustup target add wasm32-wasip2
```

## Build and Install

Lucia has two build variants: Core-only and plugin-enabled. Both produce a binary named `lucia`; the difference is whether Plugin Host, Wasmtime, plugin management, and plugin UI support are included.

### Option 1: Build Core with Cargo Only

The Core-only build excludes the WASM plugin system. It is suitable when you only need models, native tools, sessions, and events.

```bash
cargo build \
  -p lucia \
  --release \
  --no-default-features \
  --target-dir target/core-tui

./target/core-tui/release/lucia --demo
```

Install it into Cargo's binary directory:

```bash
cargo install \
  --path crates/agent-tui \
  --locked \
  --force \
  --no-default-features

lucia --demo
```

If your shell cannot find `lucia`, make sure `$HOME/.cargo/bin` is in `PATH`.

### Option 2: Build the Plugin-Enabled Version with Cargo

This build includes Plugin Host and plugin management. Building the main binary alone does not build or install the official plugins.

```bash
cargo build \
  -p lucia \
  --release \
  --features plugins \
  --target-dir target/plugin-tui

./target/plugin-tui/release/lucia --demo
```

Install the plugin-enabled binary:

```bash
cargo install \
  --path crates/agent-tui \
  --locked \
  --force \
  --features plugins
```

You can also build and load a single plugin without Bun. This example uses the Echo plugin:

```bash
cargo build \
  -p echo-plugin \
  --release \
  --target wasm32-wasip2 \
  --target-dir examples/plugins/echo-plugin/target

./target/plugin-tui/release/lucia \
  --demo \
  --plugin-manifest examples/plugins/echo-plugin/plugin.toml
```

### Option 3: Install the Plugin-enabled TUI

To use the plugin loader, install Bun and run:

```bash
bun run install:tui
lucia plugin install context
lucia --demo
```

`install:tui` only builds and installs the `lucia` binary with Plugin Host support. It does not install or enable feature plugins. Users choose capabilities with `lucia plugin search` and `lucia plugin install <id>`; official and third-party plugins use the same installation, permission, and lifecycle contracts.

To build without installing:

```bash
bun run build:tui:core
bun run build:tui:plugins
bun run build:plugin:official
```

The outputs are written to `target/core-tui/release/lucia`, `target/plugin-tui/release/lucia`, and each plugin's `target/wasm32-wasip2/release` directory.

## Configure a Model

On first launch, Lucia creates `$HOME/.lucia/config.toml`. You can also initialize the configuration and exit:

```bash
lucia --init
```

A minimal configuration looks like this:

```toml
[model]
name = "default"
provider = "open-ai"
base_url = "https://api.openai.com/v1"
model = "replace-with-an-available-model-id"
api_key_env = "OPENAI_API_KEY"
openai_protocol = "responses"

[agent]
max_steps = 0
max_tokens = 4096
stream = true

[tui]
sessions_dir = "projects"
```

`api_key_env` stores the name of an environment variable, not the key itself. Set the variable before starting Lucia:

```bash
export OPENAI_API_KEY="your-api-key"
lucia
```

The `provider` field also supports `open-ai-compatible` and `anthropic`. See [TUI configuration and sessions](docs/guide/tui-configuration.md) for local model services, custom endpoints, and the full configuration reference.

## Security Model

Open source does not make software automatically secure, and calling something a WASM sandbox does not settle every trust question. Lucia documents its boundaries instead of making promises it cannot enforce.

- Plugins must declare capabilities such as file access, native process execution, or Agent Runtime access in their manifest. Undeclared capabilities are not granted.
- Host controls plugin identity, ownership, and resource limits. It does not trust a model or guest plugin to declare authoritative values.
- `process_exec` grants a plugin native process access as the current operating-system user. Review the source and provenance before enabling a plugin with this capability.
- Requests to online models are sent to the model service you configure. Authorized plugins may also access local resources. Choose providers, plugins, and permissions according to your data requirements.

Lucia is not asking you to replace one opaque promise with another. It is trying to make trust inspectable.

## For Developers

When adding a concrete capability, prefer implementing it as a plugin. Core only owns general agent mechanics; MCP, Skills, commands, context compression, workflows, multi-agent orchestration, and specialized UI behavior belong to plugins.

The main crates keep explicit ownership boundaries:

- `agent-core`: model gateways, ReAct, context, events, and extension contracts.
- `agent-tool`: common tool types and the native tool registry.
- `agent-session`: versioned session records, CAS, and storage.
- `agent-runtime`: agent identity, spawning, lifecycle, permissions, and resource limits.
- `agent-plugin-host`: WASM ABI, authorization, contribution registration, and owner routing.
- `agent-plugin`: Guest SDK, shared protocol types, WIT bindings, and export macros.
- `agent-tui`: application assembly, configuration, input, and terminal rendering.

Common verification commands:

```bash
cargo test -p agent-core
cargo test -p lucia --no-default-features
cargo test -p lucia --features plugins
```

Plugin changes should also compile the `wasm32-wasip2` component and pass that plugin's real Host smoke tests. The official plugins expose corresponding `bun run test:plugin:*` commands in `package.json`.

## Documentation

The detailed documentation is currently maintained in Simplified Chinese:

- [Quick start](docs/guide/quick-start.md)
- [TUI usage](docs/usage/tui.md)
- [CLI usage](docs/usage/cli.md)
- [Plugin management](docs/usage/plugin-management.md)
- [Create a WASM plugin](docs/plugin/quick-start.md)
- [Plugin development](docs/development/plugin.md)
- [Manifest and capabilities](docs/host/manifest-capabilities.md)
- [Rust API reference](docs/reference/rust-api.md)
- [Architecture boundaries](docs/guide/architecture.md)

## Project Status

The repository includes an offline example, Core-only and plugin-enabled TUI builds, official plugins, end-to-end plugin smoke tests, and layered development documentation. Before a stable release, Lucia still needs broader real-world validation, security review, and feedback from different workflows.

## Getting Help and Contributing

Use GitHub Issues for reproducible bugs, focused feature proposals, and documentation problems. When contributing code:

- Keep Core mechanisms separate from concrete plugin behavior.
- Add focused tests at both sides of any changed protocol boundary.
- Run `cargo fmt --all -- --check` and the affected crate or plugin tests before opening a pull request.
- Never include API keys, tokens, private configuration, or unredacted live-test output in an issue or commit.

The goal is to build agent infrastructure that people can understand, modify, and control. Focused fixes, documentation improvements, and independent plugins are welcome.

## License

[MIT](LICENSE)
