# nunu-cli

Upload build artifacts to [nunu](https://nunu.ai) from your terminal or CI.

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

- `NUNU_API_KEY`: a nunu API key
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
- name: Upload build to nunu
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

`download_run_files` downloads selected artifacts or all artifacts from a run.
Paste a full nunu run link as `run_id`; the project is inferred and files go to
`downloads/<project>/<run>` inside the MCP workspace by default:

```json
{"run_id": "https://nunu.ai/nexus/my-project/runs/c/my-run"}
```

Raw multiplayer and composite run IDs also work: the CLI asks the authenticated
remote inspection tool for the owning project when it is not configured.
Optionally set `destination_dir` (or its `path` alias) to
choose a folder, and `files: ["logs/output.txt"]` to select filenames. Matching
files across players are kept in separate player folders. The existing
`{run_id, file_ref, relative_path}` file selectors still work; their player
`run_id` is optional. A missing run or project returns guidance instead of
guessing which run to download.
For a stored filename that is not portable, use an object selector with a safe
`relative_path` to choose its local name.

`download_run_files` is a local CLI MCP tool. It saves files directly into the
workspace and reports their local paths. The remote Nexus MCP does not expose
a download-link tool.

It transfers up to four files at a time. Each file is downloaded to a temporary
file, checked against the expected version and size, then saved from that same
open file handle without overwriting an existing path. Saving copies the
verified bytes locally. Interrupted transfers discard their temporary files,
and failed or cancelled saves remove incomplete output.
The result reports each file as `downloaded` or `failed`; a partial failure
keeps completed files. Retry failed files with `files` to download them again from
the beginning. Existing paths return per-file errors and are preserved.

Downloads require a website and Bouncer version that supports artifact download
manifests.

## Configuration

- `NUNU_API_KEY`: API key for CI or other non-interactive use
- `NUNU_PROJECT_ID`: default project ID
- `NUNU_BASE_URL`: nunu deployment URL; defaults to `https://nunu.ai`
- `NUNU_WORKSPACE_ROOT`: allowed workspace root for local MCP file uploads and run artifact downloads

The CLI also loads values from a local `.env` file.
