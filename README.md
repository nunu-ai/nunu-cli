# nunu-cli

Upload build artifacts to [Nunu](https://nunu.ai) from your terminal or CI.

## Install

Requires Node.js 18 or newer.

```bash
npm install --global @nunu-ai/nunu-cli
nunu-cli --version
```

Or run it without installing:

```bash
npx --yes @nunu-ai/nunu-cli --version
```

The npm package includes native binaries for Linux x64, macOS x64/ARM64, and
Windows x64.

## Log in

```bash
npx --yes @nunu-ai/nunu-cli auth login
```

Use `npx --yes @nunu-ai/nunu-cli auth status` to check the current login and
`npx --yes @nunu-ai/nunu-cli auth logout` to remove it. To save an API key
instead of using browser login, run:

```bash
npx --yes @nunu-ai/nunu-cli auth login --api-key
```

## Upload a build

```bash
nunu-cli upload build/app.apk \
  --project-id your-project-id \
  --name "Production 1.2.3"
```

Globs and multiple files are supported:

```bash
nunu-cli upload "build/*.apk" --project-id your-project-id --name "CI build"
```

The platform is inferred for `.apk`, `.ipa`, `.exe`, `.msi`, `.dmg`, `.pkg`,
`.deb`, `.rpm`, and `.AppImage` files. Use `--platform` for archives and other
ambiguous formats.

Run `nunu-cli upload --help` for all upload options.

## CI with an API key

CI should use an API key instead of browser login. Add the CLI to your project
so the version is pinned in your lockfile:

```bash
npm install --save-dev @nunu-ai/nunu-cli
```

Store these values in your CI provider's secrets or variables:

- `NUNU_API_KEY`: a Nunu API key
- `NUNU_PROJECT_ID`: the project receiving the build

Then upload with the local package:

```bash
npx nunu-cli upload "build/*.apk" --name "CI build"
```

GitHub Actions example:

```yaml
- uses: actions/checkout@v4
- uses: actions/setup-node@v4
  with:
    node-version: 20
    cache: npm
- run: npm ci
- name: Upload build to Nunu
  env:
    NUNU_API_KEY: ${{ secrets.NUNU_API_KEY }}
    NUNU_PROJECT_ID: ${{ vars.NUNU_PROJECT_ID }}
  run: npx nunu-cli upload "build/*.apk" --name "Build ${{ github.run_number }}"
```

Do not pass API keys as command-line arguments or commit them to `.env` files.

## MCP server

Log in once, then configure your MCP client to start the local stdio server:

```json
{
  "mcpServers": {
    "nunu": {
      "command": "npx",
      "args": ["--yes", "@nunu-ai/nunu-cli", "mcp"]
    }
  }
}
```

For non-interactive use, set `NUNU_API_KEY` in the MCP server environment. Use
`NUNU_WORKSPACE_ROOT` when file access should be rooted somewhere other than the
client's working directory.

## Configuration

- `NUNU_API_KEY`: API key for CI or other non-interactive use
- `NUNU_PROJECT_ID`: default project ID
- `NUNU_BASE_URL`: Nunu deployment URL; defaults to `https://nunu.ai`
- `NUNU_WORKSPACE_ROOT`: allowed workspace root for local MCP file uploads

The CLI also loads values from a local `.env` file.
