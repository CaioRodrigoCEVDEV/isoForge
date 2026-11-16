# ISOForge

Native Linux ISO editor written in Rust and GTK4. It opens an existing ISO,
extracts its content into a temporary workspace, lets you add/remove files, and
rebuilds a new image without modifying the original.

The reconstruction path preserves ISO9660, UDF and El Torito boot metadata for
the formats listed below. It does **not** claim that a generated image is
bootable until an actual VM boot test succeeds.

## What works

- Inspect ISO9660, ISO9660:1999, Joliet, Rock Ridge, UDF, El Torito, BIOS/UEFI
  boot images, MBR/GPT system areas, volume ID and essential boot files.
- Extract the **UDF layer as the source of truth** when present. This is
  critical for Windows images, where the ISO9660 view can expose a limited-size
  file while the full file exists only in UDF.
- Rebuild ISO9660 + UDF + El Torito with `genisoimage -udf -allow-limited-size`.
- Rebuild plain ISO9660 + El Torito with `xorriso -as mkisofs`.
- Restore BIOS and UEFI boot images byte-for-byte from the original El Torito
  LBAs. The rebuild never uses `-boot-info-table`.
- Preserve the original Volume ID, boot catalog path, load sizes and boot paths.
- Validate the generated image: UDF/ISO9660 layers, El Torito entries, boot
  image bytes, BOOTMGR/EFI/install.wim/install.esd, workspace file integrity and
  `autounattend.xml` when present.
- Batch/extraction/rebuild progress, cancellation and background work.
- Never overwrite the original ISO. The output is written to a temporary file
  and renamed only after a quick structural check.
- Workspace is created on a disk-backed cache directory, not in `/tmp` (which
  is often a small tmpfs). Free space is checked before extraction/rebuild.

## Supported formats

| Format | Inspect | Extract | Rebuild | Notes |
|---|---:|---:|---:|---|
| ISO9660 / ISO9660:1999 + El Torito | yes | yes | yes | `XorrisoBackend` |
| ISO9660 + UDF + El Torito (Windows) | yes | yes (UDF via 7z) | yes | `HybridIsoBackend` / genisoimage |
| ISO9660 without El Torito | yes | yes | yes (data ISO) | not bootable |
| UDF only (no ISO9660) | yes | read-only | **no** | safe UDF-only rebuild not implemented |
| Hybrid MBR/GPT system area | yes | yes | **blocked** | metadata would be lost; refused explicitly |

The ISO9660 layer of a hybrid Windows image may contain a *limited-size* view of
`install.wim` (for example 2.4 GB) while the UDF layer contains the full file
(6.7 GB). ISOForge extracts, rebuilds and validates through UDF in that case.
The validation report shows the ISO9660/UDF size difference as a **WARNING**,
not as a failure, because it is expected with `-allow-limited-size` and the UDF
layer carries the complete content.

## Dependencies (Ubuntu)

```bash
sudo apt update
sudo apt install build-essential pkg-config libgtk-4-dev \
  xorriso p7zip-full genisoimage
```

- `xorriso` is used for inspection and for non-UDF rebuilds.
- `7z` (p7zip-full) is mandatory to extract/inspect UDF layers.
- `genisoimage` is mandatory to rebuild UDF. xorriso/libisofs cannot write UDF.
- Install Rust with rustup if needed:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

If `genisoimage` is not available as a system package, it can be extracted
without root (as documented by the project runbook):

```bash
cd /tmp
apt-get download genisoimage
dpkg -x genisoimage_*.deb /tmp/genisoimage-tools
export ISOFORGE_GENISOIMAGE=/tmp/genisoimage-tools/usr/bin/genisoimage
```

ISOForge also checks `PATH` and the local fallback
`$HOME/.local/opt/isoforge-tools/usr/bin/genisoimage`.

## Build and run

```bash
cargo run
```

## Workflow

1. Open an ISO. ISOForge inspects it and extracts the workspace in the
   background.
2. The file tree can be expanded/collapsed; files show icons and sizes.
3. Add a file to the workspace root or remove the selected item.
4. Click **Inspecionar e exportar**, choose the destination, review the
   inspection dialog (filesystem, UDF, El Torito, BIOS/UEFI, estimated size,
   compatibility) and confirm.
5. ISOForge rebuilds the image, validates it and shows a PASS/WARNING/FAIL
   report. The final status is one of:
   - **ISO inicializável validada** — only after an actual VM boot test.
   - **ISO criada e validada estruturalmente; boot não testado em VM** — the
     normal result when QEMU/OVMF are unavailable.
   - **Reconstrução inválida — não use para boot** — at least one mandatory
     check failed.

The original ISO is never modified and the output never overwrites it.

## Tests

```bash
cargo test
```

The default suite covers ISO inspection, hybrid UDF + El Torito rebuilds,
non-UDF xorriso rebuilds, validation of missing UDF/BOOTMGR, and the file tree.

The full round trip against a real Windows ISO is intentionally ignored because
it reads/creates several gigabytes:

```bash
ISOFORGE_REAL_ISO=/path/to/Windows.iso \
  cargo test -- --ignored real_iso_roundtrip --nocapture
```

The test extracts via UDF, copies `autounattend.xml` from the ISO directory when
present, rebuilds with genisoimage, validates and compares the UDF `install.wim`
size byte count. It does not claim a boot test was executed.

## Environment variables

| Variable | Purpose |
|---|---|
| `ISOFORGE_WORK_DIR` | Workspace root (default: `$XDG_CACHE_HOME/isoforge` or `$HOME/.cache/isoforge`). |
| `ISOFORGE_GENISOIMAGE` | Explicit path to `genisoimage`. |
| `ISOFORGE_7Z` | Explicit path to `7z`. |
| `ISOFORGE_REAL_ISO` | ISO used by the ignored real-ISO test. |

## Known limitations

- No automatic VM boot test. QEMU and OVMF are not invoked; the app reports
  boot validation as WARNING until a real boot test passes. Manual QEMU/VirtualBox
  testing is still required.
- Pure UDF-only images can be inspected/opened read-only, but not rebuilt.
- Hybrid MBR/GPT/APM system areas are detected and blocked instead of being
  silently dropped. A future backend can replay them.
- The output ISO9660 layer may expose limited-size large files; the full content
  is in the UDF layer. This matches the original Windows image and is reported
  as a WARNING by the validator.
- Non-ASCII names depend on genisoimage's charset support; the current rebuild
  uses the same ISO9660:1999/UDF options as the validated Windows workflow.
- If the process is killed with SIGKILL, the temporary workspace may remain in
  `$XDG_CACHE_HOME/isoforge`; normal exit cleans it.

## Roadmap

- [x] UDF-aware extraction and reconstruction
- [x] BIOS/UEFI El Torito preservation
- [x] Structural validation report
- [x] Background workers, progress and cancellation
- [ ] Automated QEMU/OVMF boot validation
- [ ] Hybrid MBR/GPT replay backend
- [ ] Pure UDF rebuild backend
- [ ] Directory-aware add/import and persistent projects
