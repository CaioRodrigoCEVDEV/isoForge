# Packaging and release

ISOForge is packaged as a Debian package and an AppImage for Linux x86_64.
The release workflow is [`.github/workflows/release.yml`](../.github/workflows/release.yml).

The first planned publication is the **pre-release `v0.2.0`**. The workflow
never creates tags and never publishes a final (non-pre-release) release.

## CI pipeline

The `Release` workflow runs on Ubuntu 24.04 and is triggered by:

- a pushed tag matching `v*`; or
- a manual *Run workflow* with an existing tag (for example `v0.2.0`).

Jobs:

1. **Test** — checks out the tag, installs GTK4/xorriso/7z/genisoimage, runs
   `cargo fmt --all -- --check` and `cargo test --locked --all-targets`.
2. **Build DEB and AppImage** — verifies that `v<Cargo version>` matches the
   tag, builds with `cargo build --release --locked`, creates the `.deb` and
   `.AppImage`, validates the DEB layout and AppImage contents, and starts the
   AppImage under Xvfb as a smoke test.
3. **Publish pre-release** — downloads the artifacts, writes `SHA256SUMS`, and
   creates or updates the GitHub pre-release with the `.deb`, `.AppImage`, and
   checksum file. It uses `gh release create --verify-tag` / `gh release upload`
   and the scoped `GITHUB_TOKEN` (`contents: write` only in this job).

## Version and tag rules

The package version comes from `Cargo.toml` (`0.2.0`). The workflow refuses to
build when the tag does not equal `v<Cargo version>`:

```text
Cargo version: 0.2.0
Required tag:  v0.2.0
```

Do not create tags from the CI. Create them deliberately from a reviewed commit.

## DEB package

Local build:

```bash
cargo build --release --locked
bash packaging/build-deb.sh 0.2.0 dist
```

Output: `dist/isoforge_0.2.0_amd64.deb`.

The package installs:

```text
/usr/bin/isoforge
/usr/share/applications/io.github.isoforge.ISOForge.desktop
/usr/share/metainfo/io.github.isoforge.ISOForge.metainfo.xml
/usr/share/icons/hicolor/{48x48,64x64,128x128,256x256,512x512,scalable}/apps/io.github.isoforge.ISOForge.*
/usr/share/doc/isoforge/{copyright,changelog.Debian.gz}
```

Runtime dependencies are computed with `dpkg-shlibdeps` on the build host and
combined with the external tools:

- `libgtk-4-1` (and the transitive GLib/Pango/Cairo libraries)
- `libc6`, `libgcc-s1`
- `xorriso`
- `p7zip-full` (provides `7z`)
- `genisoimage`

Install:

```bash
sudo apt install ./isoforge_0.2.0_amd64.deb
```

`apt install ./file.deb` resolves the declared dependencies automatically. The
classic alternative is:

```bash
sudo dpkg -i isoforge_0.2.0_amd64.deb
sudo apt-get -f install
```

The package is built for Ubuntu 24.04 (glibc 2.39, GTK 4.14). It is not
guaranteed to install or run on older distributions.

## AppImage

Local build:

```bash
# Download the pinned tools used by the workflow:
mkdir -p tools
curl -fL -o tools/linuxdeploy \
  https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage
curl -fL -o tools/linuxdeploy-plugin-gtk.sh \
  https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/7a3fbc31a9e5075073ff8790f26effbac5f84453/linuxdeploy-plugin-gtk.sh
chmod +x tools/linuxdeploy tools/linuxdeploy-plugin-gtk.sh

cargo build --release --locked
bash packaging/build-appimage.sh 0.2.0 dist tools
```

Output: `dist/ISOForge-0.2.0-x86_64.AppImage`.

The AppImage bundles GTK4 and the GTK resources (schemas, GDK pixbuf loaders,
icon theme) through linuxdeploy and `linuxdeploy-plugin-gtk` with
`DEPLOY_GTK_VERSION=4`.

External tools are **not** bundled. The host must provide:

- `xorriso`
- `p7zip-full` / `7z`
- `genisoimage` (required for UDF rebuilds)

Run:

```bash
chmod +x ISOForge-0.2.0-x86_64.AppImage
./ISOForge-0.2.0-x86_64.AppImage
```

If FUSE is unavailable:

```bash
APPIMAGE_EXTRACT_AND_RUN=1 ./ISOForge-0.2.0-x86_64.AppImage
```

Compatibility: the release AppImage is built and smoke-tested on **Ubuntu
24.04**. It is not a generic portable build: glibc is provided by the host, so
older distributions are not guaranteed to work. The local validation on Ubuntu
26.04 is structural and functional for this host, not the release ABI.

## SHA256 verification

Download `SHA256SUMS` next to the packages, then:

```bash
sha256sum -c SHA256SUMS
```

Or compare a single file:

```bash
sha256sum ISOForge-0.2.0-x86_64.AppImage
grep ISOForge-0.2.0-x86_64.AppImage SHA256SUMS
```

## Publishing v0.2.0

The workflow is triggered by a tag. Do **not** create tags automatically and do
not use `gh release create` manually; the workflow owns the release assets and
release notes.

1. Confirm the version and tests:

   ```bash
   grep '^version' Cargo.toml      # 0.2.0
   cargo fmt --all -- --check
   cargo test --locked
   ```

2. Commit the release changes and push the branch.

3. Create and push the tag deliberately:

   ```bash
   git tag -a v0.2.0 -m "ISOForge 0.2.0"
   git push origin v0.2.0
   ```

   The tag push starts the `Release` workflow and creates the **pre-release**.

4. Alternatively, run the workflow manually:

   GitHub → Actions → **Release** → **Run workflow** → enter the existing tag
   `v0.2.0`.

   The checkout and `--verify-tag` steps fail if the tag does not exist on the
   remote. The workflow does not create it.

Expected release assets:

```text
isoforge_0.2.0_amd64.deb
ISOForge-0.2.0-x86_64.AppImage
SHA256SUMS
```

Release notes are generated by GitHub (`--generate-notes`). The release is
marked `--prerelease`; publishing a final release remains a manual decision
after boot validation on real hardware/VMs.

## Validation performed locally

The complete package pipeline was executed in an **Ubuntu 24.04.5** container,
matching the GitHub Actions runner:

- `cargo fmt --all -- --check`: passed.
- `cargo test --locked --all-targets`: 6 passed, 1 ignored (real 7.6 GB ISO).
- `cargo build --release --locked`: passed.
- DEB built and validated: `dpkg-deb --info/--contents/--extract`,
  `desktop-file-validate`, `appstreamcli validate --no-net`, `file`/`ldd`.
- AppImage built with linuxdeploy + GTK4 plugin, extracted, verified to contain
  `usr/bin/isoforge`, GTK4 libraries, desktop/icon/metainfo, and smoke-tested
  with Xvfb (the process stayed alive until the timeout).
- `SHA256SUMS` generated successfully.
- Observed DEB runtime dependencies:
  `libc6 (>= 2.39), libgcc-s1, libglib2.0-0t64, libgtk-4-1 (>= 4.10),
  libpango-1.0-0, xorriso, p7zip-full, genisoimage`.

The same scripts were also exercised on the local Ubuntu 26.04 host. The 24.04
container result is the one that matches CI.

## What still requires validation

- The GitHub Actions workflow itself has not been executed/published in this
  repository yet. The release job, `GITHUB_TOKEN` permissions, and pre-release
  creation must be observed on the first real tag push/manual run.
- The AppImage must be tested on a clean Ubuntu 24.04 desktop, including opening
  a real ISO and rebuilding it with the host `xorriso`/`7z`/`genisoimage`.
- Actual BIOS/UEFI boot tests are still manual; see the main README limitations.
- The AppImage and DEB rely on system `xorriso`, `7z`, and `genisoimage`.
- appimagetool emits a cosmetic warning about a legacy `.appdata.xml` name even
  though `usr/share/metainfo/io.github.isoforge.ISOForge.metainfo.xml` is
  included. The AppStream metadata validates and is packaged.
