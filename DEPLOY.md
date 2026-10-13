# Deploying complot

How complot gets from a git tag to a one-line install on macOS, Linux and
Windows.

| Platform | Channel | Users run | Updates |
|---|---|---|---|
| macOS | Homebrew tap `pbosetti/homebrew-mads` (shared with MADS) | `brew install pbosetti/mads/complot` | `brew upgrade` |
| Linux | `.deb` on GitHub Releases (amd64, arm64) | `sudo apt install ./complot_X.Y.Z-1_amd64.deb` | manual, or an APT repo (see below) |
| Linux | the same Homebrew tap (Homebrew on Linux) | `brew install pbosetti/mads/complot` | `brew upgrade` |
| Windows | winget, `PaoloBosetti.complot` | `winget install PaoloBosetti.complot` | `winget upgrade` |
| any | archives on GitHub Releases | download, unpack, put `complot` on `PATH` | manual |

Everything is driven by one action: pushing a tag `vX.Y.Z`. The release
workflow builds all platforms, publishes the GitHub Release, updates the
Homebrew tap and opens the winget pull request.

## How it fits together

```
git push origin vX.Y.Z
  └─ .github/workflows/release.yml
       ├─ version   tag must equal the Cargo.toml version, or the release stops
       ├─ build     5 targets, tests, archives (+ .deb on Linux)
       │            x86_64 / aarch64 Linux, x86_64 / aarch64 macOS, x86_64 Windows
       ├─ release   GitHub Release with all archives, .debs and SHA256SUMS.txt
       ├─ homebrew  renders Formula/complot.rb, pushes to pbosetti/homebrew-mads
       └─ winget    opens a PR on microsoft/winget-pkgs with the new version
```

Every other push (and every pull request) runs `.github/workflows/ci.yml`:
format check, clippy with warnings as errors, tests and a release build on
Linux, macOS and Windows.

The Homebrew and winget jobs run only when their token secrets exist, so the
release works before those channels are set up.

## Versions

`complot -V` prints the version; `complot --version` adds the build target:

```
$ complot -V
complot 0.2.0
$ complot --version
complot 0.2.0
target: aarch64-apple-darwin
```

The version is fixed at build time by `build.rs`, from the first of:

1. the `COMPLOT_VERSION` environment variable (the release workflow sets it from
   the tag);
2. `git describe --tags --match 'v[0-9]*' --dirty`: `0.2.0` on a tag,
   `0.2.0-3-gabc1234` three commits later, `-dirty` with uncommitted changes;
3. without a tag, the Cargo.toml version and the commit: `0.1.0+gabc1234`;
4. without git (e.g. a source tarball), the Cargo.toml version.

Since packagers may build from a tarball without `.git`, the Cargo.toml version
must match the tag. The release workflow enforces it.

## One-time setup

### 1. The GitHub repository

The repository must be **public**: the free arm64 Linux runners
(`ubuntu-22.04-arm`), Homebrew and winget all need public downloads.

```sh
gh repo create pbosetti/complot --public --source . --push
```

The first push runs CI on all three systems. The Windows code has never been
compiled before this, so expect to fix that run first.

### 2. Homebrew tap

complot lives in the existing tap
[pbosetti/homebrew-mads](https://github.com/pbosetti/homebrew-mads), next to
the MADS formula: a tap can hold any number of formulae. Users install it with
`brew install pbosetti/mads/complot`, and anyone who has already tapped
`pbosetti/mads` sees complot after a `brew update`.

The release workflow only ever writes `Formula/complot.rb`; `Formula/mads.rb`,
`update.sh` and the tap's README are left alone. Add a complot section to that
README by hand once:

```md
## complot

[complot](https://github.com/pbosetti/complot): real-time plots from the shell
(macOS and Linux).

    brew install pbosetti/mads/complot

`Formula/complot.rb` is updated automatically by complot's release workflow.
```

Then create a **fine-grained personal access token** (GitHub → Settings →
Developer settings → Fine-grained tokens) restricted to the repository
`pbosetti/homebrew-mads`, with **Contents: Read and write**, and store it in
the complot repository:

```sh
gh secret set HOMEBREW_TAP_TOKEN --repo pbosetti/complot
```

From the next release on, the `homebrew` job writes `Formula/complot.rb` with
the new version and checksums and pushes it to the tap.

The formula installs the prebuilt binary for the machine (Apple Silicon or
Intel macOS, arm64 or x86_64 Linux); nothing is compiled on the user's machine.
Homebrew doesn't quarantine formula downloads, so the unsigned binary runs
without Gatekeeper prompts.

To publish a formula by hand (e.g. for a release made before the token
existed):

```sh
gh release download vX.Y.Z --repo pbosetti/complot --pattern SHA256SUMS.txt
packaging/homebrew/render-formula.sh X.Y.Z SHA256SUMS.txt > complot.rb
# copy complot.rb to Formula/ in a clone of homebrew-mads, commit, push
brew audit --strict --online pbosetti/mads/complot
brew install pbosetti/mads/complot && brew test complot
```

**Later: homebrew-core.** Once complot is better known (Homebrew asks for a
project with some traction, roughly 75+ stars or 30+ forks), it can move to
homebrew-core with `brew install complot`. Core formulae must build from
source: `depends_on "rust" => :build` and
`system "cargo", "install", *std_cargo_args`. Its formula can then be removed
from the tap.

### 3. Linux packages

Nothing to set up: the release workflow builds `.deb` packages with
[cargo-deb](https://github.com/kornelski/cargo-deb), from
`[package.metadata.deb]` in Cargo.toml, and attaches them to the release:

- `complot_X.Y.Z-1_amd64.deb` and `complot_X.Y.Z-1_arm64.deb`;
- built on Ubuntu 22.04, so they run on Ubuntu 22.04+, Debian 12+ and their
  derivatives (Mint, Pop!_OS, …);
- they install `/usr/bin/complot` plus the README and licenses in
  `/usr/share/doc/complot/`.

Users install them with apt, which also pulls in the dependencies:

```sh
sudo apt install ./complot_X.Y.Z-1_amd64.deb
```

complot needs a desktop session (X11 or Wayland) and a working GPU driver
(Vulkan, or OpenGL/EGL; Mesa is fine). The window libraries are loaded at run
time, so the package lists them explicitly (`libxkbcommon0`, `libegl1 | libgl1`,
recommends `libvulkan1`, `libwayland-client0`, `libx11-6`, …).

To check a package before announcing a release:

```sh
docker run --rm -v "$PWD":/d ubuntu:22.04 sh -c \
  'apt-get update && apt-get install -y /d/complot_X.Y.Z-1_amd64.deb && complot -V'
```

**Later: an APT repository**, so that `apt upgrade` picks up new versions. The
options, from least to most work:

- [Cloudsmith](https://cloudsmith.io) or [packagecloud](https://packagecloud.io):
  free for open-source projects. Upload the `.deb` files from the release
  workflow with their CLI and an API key secret; users add the repository
  once.
- A repository on GitHub Pages, built with `reprepro` or `aptly` and signed
  with a dedicated GPG key kept as a secret. Free and self-contained, but the
  signing key has to be looked after.
- A Launchpad PPA: Ubuntu only, and it needs source packages built on
  Launchpad, which is a much bigger change.

Fedora and openSUSE users can use the archive or Homebrew for now. An `.rpm`
via `cargo-generate-rpm` can be added to the same workflow later.

### 4. winget

winget packages live in
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) as YAML
manifests. complot ships as a zip with a portable executable: winget unpacks
it and puts `complot` on the `PATH`.

The **first version is submitted by hand**; later versions are automated.

1. Make the first release (see the checklist below) and download its sums:
   ```sh
   gh release download vX.Y.Z --repo pbosetti/complot --pattern SHA256SUMS.txt
   packaging/winget/render-manifests.sh X.Y.Z SHA256SUMS.txt manifests
   ```
2. On a Windows machine, validate and try the manifests:
   ```powershell
   winget validate --manifest manifests
   winget settings --enable LocalManifestFiles   # in an admin shell, once
   winget install --manifest manifests
   complot -V
   ```
   Also check that a plot window opens, receives data and closes, since this
   is where the Windows build gets its first real use.
3. Submit them, either with
   [wingetcreate](https://github.com/microsoft/winget-create)
   (`wingetcreate submit manifests`) or [Komac](https://github.com/russellbanks/Komac),
   or by forking microsoft/winget-pkgs and adding the three files under
   `manifests/p/PaoloBosetti/complot/X.Y.Z/`. Automated checks (installation,
   malware scan) run on the pull request; a moderator merges it, usually within
   a few days.
4. Once it is merged, enable automation:
   - fork microsoft/winget-pkgs under `pbosetti` (the action pushes branches
     there);
   - create a **classic** personal access token with the `public_repo` scope
     (the action needs it to open PRs on another owner's repository);
   - `gh secret set WINGET_TOKEN --repo pbosetti/complot`.

From then on the `winget` job opens the update PR on every release.

Users install with `winget install PaoloBosetti.complot`. Once the moniker
`complot` is accepted, `winget install complot` works too.

## Releasing a version

1. Make sure CI is green on `main`.
2. Bump `version` in Cargo.toml, then run `cargo build` so Cargo.lock follows.
3. Commit, then tag and push:
   ```sh
   git tag -a v0.2.0 -m "complot 0.2.0"
   git push origin main v0.2.0
   ```
4. Watch the release: `gh run watch --repo pbosetti/complot`.
5. Check the results:
   - the release page lists 5 archives, 2 `.deb` files and `SHA256SUMS.txt`;
   - `brew update && brew upgrade complot && complot -V` (if the tap is set up);
   - a new PR on microsoft/winget-pkgs (once winget is automated).

A tag with a pre-release suffix (`v0.2.0-rc.1`, with the same version in
Cargo.toml) is published as a pre-release on the release page only: the
Homebrew and winget jobs skip it. To redo a failed release, delete the release
and the tag, fix, and tag again:

```sh
gh release delete v0.2.0 --repo pbosetti/complot --yes
git push --delete origin v0.2.0 && git tag -d v0.2.0
```

## Signing (optional, later)

The binaries are not code-signed. That matters only outside the package
managers:

- **macOS**: a binary downloaded from the release page with a browser is
  quarantined; `xattr -d com.apple.quarantine complot` clears it. Homebrew
  installs are not affected. Signing and notarizing needs an Apple Developer ID
  ($99/year), then `codesign` and `xcrun notarytool` in the macOS build job.
- **Windows**: SmartScreen may warn about an unsigned executable downloaded
  directly. Options are [SignPath](https://signpath.org) (free for open-source
  projects) or Azure Trusted Signing (a few dollars a month).

## Files

| File | Role |
|---|---|
| `build.rs` | Version from the tag, `COMPLOT_VERSION` or Cargo.toml |
| `Cargo.toml` | Package metadata, `[package.metadata.deb]` for the `.deb` |
| `LICENSE-MIT`, `LICENSE-APACHE` | The dual license, shipped in every package |
| `.github/workflows/ci.yml` | Lint, test and build on every push and PR |
| `.github/workflows/release.yml` | Build, release and publish on `v*` tags |
| `packaging/homebrew/complot.rb.in` | Formula template |
| `packaging/homebrew/render-formula.sh` | Fills in version and checksums |
| `packaging/winget/*.yaml` | Manifest templates for the first winget submission |
| `packaging/winget/render-manifests.sh` | Fills in version and checksum |

## Known risks

- **Windows has never been built.** The first CI run is the first compile of
  the `#[cfg(windows)]` code, and nobody has opened a window on Windows yet.
  Do step 2 of the winget setup on a real Windows machine before submitting.
- **arm64 Linux runners** (`ubuntu-22.04-arm`) are free only for public
  repositories. For a private repository, remove that row from the release
  matrix or build it with `cross`.
- **Runner images age out.** The Linux release rows use `ubuntu-22.04` for its
  older glibc, so the binaries run on more distributions. When GitHub retires
  that image, switch both rows to `ubuntu-24.04` / `ubuntu-24.04-arm`; the
  packages then need Ubuntu 24.04 or Debian 13.
- **Windows builds run under Git Bash** (`shell: bash`). If linking ever fails
  with `link: extra operand`, Git's `link` is shadowing MSVC's: give the
  Windows build steps `shell: pwsh`.
- **Tag and Cargo.toml must agree**; the release fails fast otherwise, before
  anything is published.
