# SDK Release Guide (PyPI / npm)

<p align="center">  <a href="release.md">中文</a> · <b>English</b>  </p>

The Python SDK and the TS SDK are published automatically by
`.github/workflows/release-sdks.yml`: **tag `v*` driven + OIDC Trusted
Publishing (no tokens stored in repo secrets)**. The CLI binary release
(`release.yml` → GitHub Release) is triggered by the same tag; the two
pipelines are independent.

Versions are managed in **four places** (one version number across the
Rust workspace / Python pyproject / Python `__version__` / TS
package.json) via `scripts/release_sdk.sh`:

```bash
./scripts/release_sdk.sh check        # version consistency + build both packages locally + metadata checks (no publish)
./scripts/release_sdk.sh bump 0.2.0   # sync the bump in all four places (idempotent); commit the change
./scripts/release_sdk.sh tag 0.2.0    # real release: tag v0.2.0 and push (⚠️ triggers both pipelines below)
```

## Pipeline Structure

`release-sdks.yml` has 8 jobs:

| job | what it does |
|---|---|
| `dispatch-guard` | trigger guard (only main / tags may trigger) |
| `gate` | four-place version consistency check + offline tests for both SDKs (mock, no network); on failure, all later jobs are skipped |
| `binaries` | compile `zhuye-cli` release binaries for 5 platforms |
| `npm-platform` | assemble the 5 platform sub-packages (binary + generated manifest) |
| `pypi` | 5 platform wheels (maturin auto platform tags, embedded binary) |
| `pypi-sdist` | pure source sdist + `twine check` |
| `pypi-publish` | upload all wheels / sdist to PyPI |
| `npm` | publish the main `zhuye` package (injects `optionalDependencies` dynamically before publish) |

The platform matrix is 5: `darwin-arm64` / `darwin-x64` / `linux-x64` /
`linux-arm64` / `win32-x64`.

Five artifact classes are produced: `binary-<slug>` /
`npm-platform-<slug>` / `pypi-wheel-<slug>` / `pypi-sdist` / `npm-pack`.

The npm side publishes **6 packages** in total: the main `zhuye` plus 5
platform sub-packages `zhuye-<os>-<arch>`
(`zhuye-darwin-arm64` / `zhuye-darwin-x64` / `zhuye-linux-x64` /
`zhuye-linux-arm64` / `zhuye-win32-x64`); the main package pins them per
platform via `optionalDependencies`. **Every sub-package must be
registered as a Trusted Publisher separately** — registering only the
main package is not enough.

## Testing Phase (Current State)

The platform side (PyPI / npm accounts and Trusted Publishing
registration) is **not configured yet**, so today we only verify the
dry-run path of the pipelines — **no publishing, no package names
consumed**:

1. Run `./scripts/release_sdk.sh check` locally and confirm both
   packages build and pass the metadata checks;
2. After pushing, trigger manually on GitHub under **Actions →
   release-sdks → Run workflow**, keeping `dry_run` at its default
   `true`;
3. Verify all jobs in the run are green, then download
   `pypi-wheel-<slug>`, `pypi-sdist`, `npm-pack` and
   `npm-platform-<slug>` to inspect package contents and metadata by hand.

⚠️ In the testing phase, **do not push a `v*` tag**: that would trigger
both `release.yml` (CLI published as a public GitHub Release) and
`release-sdks.yml` (real SDK publish; it fails while the platforms are
unconfigured).

## Release Checklist (package name settled as `zhuye`)

### Step 0: Package Name

**`zhuye`** on both PyPI / npm (both confirmed unoccupied). The
published name matches the Python import name (`import zhuye`) and the
npm import name (`import { ZhuyeAgent } from 'zhuye'`).

### Step 1: PyPI Side (Pending Publisher)

1. Register a [pypi.org](https://pypi.org) account;
2. Account settings → Publishing → add a **Pending Publisher** (the
   project may not exist yet; the first publish creates it
   automatically). The five-tuple must match the workflow exactly:
   - PyPI project name: `zhuye`
   - Owner: `Asketisch` / Repository: `ZhuYe`
   - Workflow filename: `release-sdks.yml`
   - Environment: `pypi`

### Step 2: npm Side (Trusted Publisher)

npm's Trusted Publisher registration lives in the **package detail
page** settings and requires the package to exist, so the first
version must be published manually once (everything after that goes
through CI):

```bash
cd sdks/typescript && npm whoami   # npm ≥ 11.5.1; run npm login first
npm publish                        # publish the first version manually
```

Then register on the npmjs.com package settings page → Trusted
Publisher (after saving, **a successful publish must complete within
2 days** or the registration expires and must be recreated): Owner
`Asketisch` / Repository `ZhuYe` / Workflow `release-sdks.yml` /
Environment `npm`. **Each of the 5 platform sub-packages
`zhuye-<os>-<arch>` must be registered separately.**

### Step 3: Publish

```bash
./scripts/release_sdk.sh bump X.Y.Z && git commit -m "build: 版本对齐 X.Y.Z"
./scripts/release_sdk.sh check
./scripts/release_sdk.sh tag vX.Y.Z    # interactive confirmation; the script verifies clean tree + version consistency
```

After pushing the tag, watch the two workflows on the Actions page:
`release` (CLI → GitHub Release) and `release-sdks` (SDK → PyPI / npm).
If a publish to either registry fails, fix it and re-run only the
corresponding job of `release-sdks` (the tag does not need to be
re-created).

## Rename Checklist

The package name is settled as `zhuye`; this section is only for a
future rename. When the package / repository name changes, the places
to update (pipelines and scripts do not hard-code the name, so no
changes are needed there):

| Place | file / action |
|---|---|
| PyPI package name | `[project] name` in `sdks/python/pyproject.toml` |
| npm main package name | `name` in `sdks/typescript/package.json` |
| npm platform sub-package names | the `zhuye-${slug}` generation logic in the `npm-platform` job + the main package's `optionalDependencies` |
| repository URL (three places) | root `Cargo.toml` `[workspace.package] repository`, `pyproject.toml` `[project.urls]`, `package.json` `repository`/`homepage`/`bugs` |
| Python package import name (if it changes) | `sdks/python/zhuye/` directory name + `pyproject.toml` `[tool.setuptools]` include |
| this document | the names used in the Step 1/2 registrations above |
| platform side | re-register PyPI Pending Publisher / npm Trusted Publisher under the new names |

Note: once a name is published to real PyPI / npm it is **occupied**;
renaming means abandoning the old name (npm allows unpublish within 72
hours to release it; PyPI does not).

## Install Smoke Tests Built into the Pipeline

Beyond the build checks, `release-sdks.yml` has two layers of
install verification to catch "built but won't install / missing files /
entry import failure" class issues:

- **Before publish (runs on dry-run too)**: Python does a clean-venv
  install of the local wheel, asserting `import zhuye` + **the embedded
  binary resolves and runs `--version`**; npm installs the real packed
  tgz into a clean directory, asserts the ESM `import('zhuye')` exports
  `ZhuyeAgent`, and — when the platform sub-package is already
  resolvable — additionally runs the embedded binary's `--version`
  (on dry-run the sub-package is not yet published, so that assertion
  is skipped and left to post-publish verification);
- **After publish (real releases only)**: actually install
  `zhuye==<tag version>` from the PyPI / npm registries; the Python
  side additionally asserts `__version__` matches the tag (registry
  CDNs have propagation lag, so retries are built in). On failure,
  re-run only the corresponding job on the Actions page — the tag does
  not need to be re-created.
