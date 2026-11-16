use crate::core::error::CoreResult;
use crate::core::fsutil::tree_files;
use crate::core::iso_inspector::{list_files, list_files_iso9660, read_boot_image};
use crate::core::model::{find_file, IsoFile, IsoMetadata};
use crate::core::process::locate_tool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warning,
    Fail,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warning => "WARNING",
            CheckStatus::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidationCheck {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl ValidationCheck {
    fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        status: CheckStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalStatus {
    /// Only set after an actual VM boot test passes.
    BootValidated,
    /// Structure and boot metadata check out, but no VM boot test was run.
    StructurallyValid,
    /// A mandatory check failed. The image must not be advertised as bootable.
    Invalid,
}

impl FinalStatus {
    pub fn label(self) -> &'static str {
        match self {
            FinalStatus::BootValidated => "ISO inicializável validada",
            FinalStatus::StructurallyValid => {
                "ISO criada e validada estruturalmente; boot não testado em VM"
            }
            FinalStatus::Invalid => "Reconstrução inválida — não use para boot",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidationReport {
    pub output: PathBuf,
    pub checks: Vec<ValidationCheck>,
    pub final_status: FinalStatus,
}

impl ValidationReport {
    pub fn invalid(output: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self {
            output: output.into(),
            checks: vec![ValidationCheck::new(
                "fatal",
                "Validação",
                CheckStatus::Fail,
                detail,
            )],
            final_status: FinalStatus::Invalid,
        }
    }

    pub fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
    }

    pub fn has_warnings(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == CheckStatus::Warning)
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut warning = 0;
        let mut fail = 0;
        for check in &self.checks {
            match check.status {
                CheckStatus::Pass => pass += 1,
                CheckStatus::Warning => warning += 1,
                CheckStatus::Fail => fail += 1,
            }
        }
        (pass, warning, fail)
    }
}

#[derive(Clone, Debug, Default)]
pub struct VmAvailability {
    pub qemu: Option<PathBuf>,
    pub ovmf: Option<PathBuf>,
}

impl VmAvailability {
    pub fn is_available(&self) -> bool {
        self.qemu.is_some() && self.ovmf.is_some()
    }
}

pub fn vm_availability() -> VmAvailability {
    let qemu = locate_tool("qemu-system-x86_64");
    let ovmf_candidates = [
        "/usr/share/OVMF/OVMF_CODE.fd",
        "/usr/share/OVMF/OVMF_CODE_4M.fd",
        "/usr/share/edk2/ovmf/OVMF_CODE.fd",
        "/usr/share/edk2/x64/OVMF_CODE.fd",
        "/usr/share/qemu/OVMF_CODE.fd",
    ];
    let ovmf = ovmf_candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file());
    VmAvailability { qemu, ovmf }
}

fn check(
    checks: &mut Vec<ValidationCheck>,
    id: &str,
    label: &str,
    condition: bool,
    pass_detail: impl Into<String>,
    fail_detail: impl Into<String>,
) {
    checks.push(ValidationCheck::new(
        id,
        label,
        if condition {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        if condition {
            pass_detail.into()
        } else {
            fail_detail.into()
        },
    ));
}

fn find_rebuilt_file<'a>(files: &'a [IsoFile], path: &str) -> Option<&'a IsoFile> {
    find_file(files, path)
}

fn check_required_file(
    checks: &mut Vec<ValidationCheck>,
    files: &[IsoFile],
    path: &str,
    label: &str,
) {
    let present = find_rebuilt_file(files, path).is_some();
    check(
        checks,
        &format!("file:{path}"),
        label,
        present,
        format!("{path} presente"),
        format!("{path} ausente na ISO reconstruída"),
    );
}

pub fn validate(
    original: &Path,
    rebuilt: &Path,
    original_meta: &IsoMetadata,
    rebuilt_meta: &IsoMetadata,
    workspace: &Path,
) -> CoreResult<ValidationReport> {
    let mut checks = Vec::new();

    checks.push(ValidationCheck::new(
        "structure:file",
        "Arquivo de saída",
        CheckStatus::Pass,
        format!(
            "{} ({})",
            rebuilt.display(),
            human_size(std::fs::metadata(rebuilt).map(|m| m.len()).unwrap_or(0))
        ),
    ));

    check(
        &mut checks,
        "fs:iso9660",
        "Camada ISO9660",
        original_meta.has_iso9660 == rebuilt_meta.has_iso9660,
        if rebuilt_meta.has_iso9660 {
            "ISO9660 presente"
        } else {
            "ISO9660 ausente nas duas imagens"
        },
        "A presença de ISO9660 difere da imagem original",
    );

    let udf_ok = if original_meta.has_udf {
        rebuilt_meta.has_udf
    } else {
        true
    };
    checks.push(ValidationCheck::new(
        "fs:udf",
        "Camada UDF",
        if udf_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        if original_meta.has_udf {
            if rebuilt_meta.has_udf {
                format!(
                    "UDF {} preservado",
                    rebuilt_meta.udf_version.as_deref().unwrap_or("detectado")
                )
            } else {
                "UDF presente na original, mas ausente na reconstrução".to_string()
            }
        } else if rebuilt_meta.has_udf {
            "UDF não existia na original; foi adicionado".to_string()
        } else {
            "UDF não faz parte desta imagem".to_string()
        },
    ));

    if let (Some(expected), Some(actual)) = (&original_meta.udf_version, &rebuilt_meta.udf_version)
    {
        if expected != actual {
            checks.push(ValidationCheck::new(
                "fs:udf-version",
                "Versão UDF",
                CheckStatus::Warning,
                format!("Versão mudou de {expected} para {actual}"),
            ));
        }
    }

    let torito_ok = !original_meta.has_el_torito || rebuilt_meta.has_el_torito;
    check(
        &mut checks,
        "boot:el-torito",
        "El Torito",
        torito_ok,
        if rebuilt_meta.has_el_torito {
            "Catálogo El Torito presente"
        } else {
            "Original não possui El Torito"
        },
        "El Torito presente na original, mas ausente na reconstrução",
    );

    if let Some(catalog) = &rebuilt_meta.boot_catalog {
        checks.push(ValidationCheck::new(
            "boot:catalog",
            "Catálogo de boot",
            CheckStatus::Pass,
            format!("{catalog} (LBA {:?})", rebuilt_meta.boot_catalog_lba),
        ));
    } else if original_meta.has_el_torito {
        checks.push(ValidationCheck::new(
            "boot:catalog",
            "Catálogo de boot",
            CheckStatus::Fail,
            "Catálogo El Torito não localizado na reconstrução",
        ));
    }

    for expected_entry in &original_meta.boot_entries {
        if !expected_entry.bootable || (!expected_entry.is_bios() && !expected_entry.is_uefi()) {
            continue;
        }
        let actual_entry = rebuilt_meta.boot_entries.iter().find(|candidate| {
            candidate.is_bios() == expected_entry.is_bios()
                && candidate.is_uefi() == expected_entry.is_uefi()
                && candidate.image_path.as_deref().map(str::to_ascii_lowercase)
                    == expected_entry
                        .image_path
                        .as_deref()
                        .map(str::to_ascii_lowercase)
        });

        let label = format!("Boot {}", expected_entry.platform);
        let Some(actual_entry) = actual_entry else {
            checks.push(ValidationCheck::new(
                format!("boot:{}", expected_entry.platform.to_ascii_lowercase()),
                label,
                CheckStatus::Fail,
                format!(
                    "Entrada ausente (esperada {} / {})",
                    expected_entry.platform,
                    expected_entry
                        .image_path
                        .as_deref()
                        .unwrap_or("sem caminho")
                ),
            ));
            continue;
        };

        if actual_entry.load_size != expected_entry.load_size {
            checks.push(ValidationCheck::new(
                format!("boot:{}:size", expected_entry.platform.to_ascii_lowercase()),
                label,
                CheckStatus::Fail,
                format!(
                    "Tamanho de carga mudou de {} para {} setores",
                    expected_entry.load_size, actual_entry.load_size
                ),
            ));
            continue;
        }

        match (
            read_boot_image(original, expected_entry),
            read_boot_image(rebuilt, actual_entry),
        ) {
            (Ok(expected_bytes), Ok(actual_bytes)) => {
                if expected_bytes == actual_bytes {
                    checks.push(ValidationCheck::new(
                        format!(
                            "boot:{}:image",
                            expected_entry.platform.to_ascii_lowercase()
                        ),
                        format!("Imagem de boot {}", expected_entry.platform),
                        CheckStatus::Pass,
                        format!(
                            "Idêntica à original ({} bytes) — {}",
                            actual_bytes.len(),
                            expected_entry
                                .image_path
                                .as_deref()
                                .unwrap_or("sem caminho")
                        ),
                    ));
                } else {
                    checks.push(ValidationCheck::new(
                        format!(
                            "boot:{}:image",
                            expected_entry.platform.to_ascii_lowercase()
                        ),
                        format!("Imagem de boot {}", expected_entry.platform),
                        CheckStatus::Fail,
                        "Os bytes da imagem de boot diferem da original".to_string(),
                    ));
                }
            }
            (Err(err), _) | (_, Err(err)) => {
                checks.push(ValidationCheck::new(
                    format!(
                        "boot:{}:image",
                        expected_entry.platform.to_ascii_lowercase()
                    ),
                    format!("Imagem de boot {}", expected_entry.platform),
                    CheckStatus::Fail,
                    format!("Não foi possível comparar a imagem de boot: {err}"),
                ));
            }
        }
    }

    let rebuilt_files = match list_files(rebuilt, rebuilt_meta) {
        Ok(files) => files,
        Err(err) => {
            checks.push(ValidationCheck::new(
                "files:list",
                "Listagem da ISO reconstruída",
                CheckStatus::Fail,
                format!("Não foi possível listar os arquivos: {err}"),
            ));
            Vec::new()
        }
    };

    if original_meta.has_udf {
        for path in ["/sources/install.wim", "/sources/install.esd"] {
            let Some(original_file) = original_meta
                .essential_files
                .iter()
                .find(|file| file.path.eq_ignore_ascii_case(path) && file.present)
            else {
                continue;
            };
            if let Some(rebuilt_file) = find_rebuilt_file(&rebuilt_files, path) {
                let ok = original_file.size == Some(rebuilt_file.size);
                checks.push(ValidationCheck::new(
                    "udf:install",
                    "Conteúdo UDF (imagem de instalação)",
                    if ok {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                    if ok {
                        format!("{} preservado com {} bytes", path, rebuilt_file.size)
                    } else {
                        format!(
                            "{} mudou de {:?} para {} bytes; a camada UDF pode estar truncada",
                            path, original_file.size, rebuilt_file.size
                        )
                    },
                ));
            }
        }
    }

    if original_meta.has_windows_layout() {
        check_required_file(&mut checks, &rebuilt_files, "/bootmgr", "BOOTMGR");
        let efi_present = find_rebuilt_file(&rebuilt_files, "/efi/boot/bootx64.efi").is_some()
            || find_rebuilt_file(&rebuilt_files, "/efi/microsoft/boot/bootmgfw.efi").is_some();
        check(
            &mut checks,
            "file:efi-boot",
            "Boot EFI (BOOTX64.EFI/BOOTMGFW.EFI)",
            efi_present,
            "Arquivo de boot EFI presente",
            "Arquivo de boot EFI ausente",
        );
        let wim_present = find_rebuilt_file(&rebuilt_files, "/sources/install.wim").is_some()
            || find_rebuilt_file(&rebuilt_files, "/sources/install.esd").is_some();
        check(
            &mut checks,
            "file:install",
            "INSTALL.WIM/INSTALL.ESD",
            wim_present,
            "Imagem de instalação presente",
            "INSTALL.WIM/INSTALL.ESD ausente",
        );
    }

    if original_meta.has_bios_boot {
        check_required_file(
            &mut checks,
            &rebuilt_files,
            "/boot/etfsboot.com",
            "Imagem BIOS (etfsboot.com)",
        );
    }
    if original_meta.has_uefi_boot {
        check_required_file(
            &mut checks,
            &rebuilt_files,
            "/efi/microsoft/boot/efisys.bin",
            "Imagem UEFI (efisys.bin)",
        );
    }

    let workspace_files = tree_files(workspace)?;
    let rebuilt_index: HashMap<String, u64> = rebuilt_files
        .iter()
        .map(|file| (file.path.to_ascii_lowercase(), file.size))
        .collect();

    let mut missing = Vec::new();
    let mut size_mismatch = Vec::new();
    let mut large_unverified = Vec::new();
    for file in &workspace_files {
        let path = file.relative_path.to_ascii_lowercase();
        if path == "/boot.catalog" {
            continue;
        }
        match rebuilt_index.get(&path) {
            Some(size) if *size == file.size => {}
            Some(size) => {
                if file.size > u32::MAX as u64 {
                    large_unverified.push(format!(
                        "{} (workspace {}, ISO9660 {}, limite de 4 GiB)",
                        file.relative_path, file.size, size
                    ));
                } else {
                    size_mismatch.push(format!(
                        "{} (workspace {}, ISO {})",
                        file.relative_path, file.size, size
                    ));
                }
            }
            None => missing.push(file.relative_path.clone()),
        }
    }

    if missing.is_empty() && size_mismatch.is_empty() {
        checks.push(ValidationCheck::new(
            "files:workspace",
            "Integridade dos arquivos do workspace",
            CheckStatus::Pass,
            format!(
                "{} arquivos do workspace presentes na ISO com os tamanhos esperados",
                workspace_files.len()
            ),
        ));
    } else {
        let mut detail = Vec::new();
        if !missing.is_empty() {
            detail.push(format!("ausentes: {}", missing.join(", ")));
        }
        if !size_mismatch.is_empty() {
            detail.push(format!(
                "tamanhos divergentes: {}",
                size_mismatch.join(", ")
            ));
        }
        checks.push(ValidationCheck::new(
            "files:workspace",
            "Integridade dos arquivos do workspace",
            CheckStatus::Fail,
            detail.join(" | "),
        ));
    }

    if !large_unverified.is_empty() {
        checks.push(ValidationCheck::new(
            "files:large",
            "Arquivos maiores que 4 GiB",
            CheckStatus::Warning,
            format!(
                "Tamanho real não verificável pela camada ISO9660 (multi-extent/UDF): {}",
                large_unverified.join(", ")
            ),
        ));
    }

    if rebuilt_meta.has_udf {
        match list_files_iso9660(rebuilt) {
            Ok(iso_files) => {
                let iso_index: HashMap<String, u64> = iso_files
                    .iter()
                    .map(|file| (file.path.to_ascii_lowercase(), file.size))
                    .collect();
                let mut differences = Vec::new();
                for udf_file in &rebuilt_files {
                    if let Some(iso_size) = iso_index.get(&udf_file.path.to_ascii_lowercase()) {
                        if *iso_size != udf_file.size {
                            differences.push(format!(
                                "{}: ISO9660 {}, UDF {}",
                                udf_file.path, iso_size, udf_file.size
                            ));
                            if differences.len() >= 6 {
                                break;
                            }
                        }
                    }
                }
                if differences.is_empty() {
                    checks.push(ValidationCheck::new(
                        "layers:iso-udf",
                        "Consistência ISO9660/UDF",
                        CheckStatus::Pass,
                        "As duas camadas apresentam os mesmos tamanhos de arquivo",
                    ));
                } else {
                    checks.push(ValidationCheck::new(
                        "layers:iso-udf",
                        "Consistência ISO9660/UDF",
                        CheckStatus::Warning,
                        format!(
                            "A camada ISO9660 usa tamanhos limitados (esperado com -allow-limited-size); \
                             a camada UDF é a fonte de verdade: {}",
                            differences.join("; ")
                        ),
                    ));
                }
            }
            Err(err) => checks.push(ValidationCheck::new(
                "layers:iso-udf",
                "Consistência ISO9660/UDF",
                CheckStatus::Warning,
                format!("Não foi possível comparar as camadas: {err}"),
            )),
        }
    }

    let autounattend_in_workspace = workspace_files
        .iter()
        .any(|file| file.relative_path.eq_ignore_ascii_case("/autounattend.xml"));
    if autounattend_in_workspace {
        check_required_file(
            &mut checks,
            &rebuilt_files,
            "/autounattend.xml",
            "autounattend.xml",
        );
    }

    if original_meta.has_system_area && !rebuilt_meta.has_system_area {
        checks.push(ValidationCheck::new(
            "fs:system-area",
            "Área de sistema MBR/GPT",
            CheckStatus::Fail,
            "A reconstrução perdeu a área de sistema da imagem original".to_string(),
        ));
    }

    let vm = vm_availability();
    if vm.is_available() {
        checks.push(ValidationCheck::new(
            "boot:vm",
            "Teste de boot em VM",
            CheckStatus::Warning,
            "QEMU e OVMF disponíveis, mas a execução automática de boot ainda não foi implementada. Faça o teste manual.",
        ));
    } else {
        let mut missing = Vec::new();
        if vm.qemu.is_none() {
            missing.push("QEMU");
        }
        if vm.ovmf.is_none() {
            missing.push("OVMF");
        }
        checks.push(ValidationCheck::new(
            "boot:vm",
            "Teste de boot em VM",
            CheckStatus::Warning,
            format!(
                "Não executado: {}. A presença dos arquivos de boot não prova que a ISO inicializa.",
                missing.join(" e ")
            ),
        ));
    }

    let final_status = if checks.iter().any(|check| check.status == CheckStatus::Fail) {
        FinalStatus::Invalid
    } else if checks
        .iter()
        .any(|check| check.id == "boot:vm" && check.status == CheckStatus::Pass)
    {
        FinalStatus::BootValidated
    } else {
        FinalStatus::StructurallyValid
    };

    Ok(ValidationReport {
        output: rebuilt.to_path_buf(),
        checks,
        final_status,
    })
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::CoreError;
    use crate::core::iso_inspector::inspect;
    use std::fs;
    use std::process::Command;

    fn write_tree(root: &Path, with_bootmgr: bool) -> CoreResult<()> {
        fs::create_dir_all(root.join("boot"))?;
        fs::create_dir_all(root.join("efi/microsoft/boot"))?;
        fs::create_dir_all(root.join("efi/boot"))?;
        fs::create_dir_all(root.join("sources"))?;
        fs::write(root.join("boot/etfsboot.com"), vec![0x90u8; 4096])?;
        fs::write(
            root.join("efi/microsoft/boot/efisys.bin"),
            vec![0x42u8; 1474560],
        )?;
        if with_bootmgr {
            fs::write(root.join("bootmgr"), b"bootmgr")?;
        }
        fs::write(root.join("efi/boot/bootx64.efi"), b"bootx64")?;
        fs::write(root.join("sources/install.wim"), vec![0x5Au8; 100_000])?;
        fs::write(root.join("autounattend.xml"), b"<unattend/>")?;
        Ok(())
    }

    fn build_hybrid(tree: &Path, output: &Path) -> CoreResult<()> {
        let genisoimage = crate::core::process::locate_tool("genisoimage")
            .ok_or_else(|| CoreError::new("genisoimage ausente"))?;
        let status = Command::new(genisoimage)
            .args([
                "-udf",
                "-allow-limited-size",
                "-iso-level",
                "4",
                "-D",
                "-N",
                "-b",
                "boot/etfsboot.com",
                "-no-emul-boot",
                "-boot-load-size",
                "8",
                "-eltorito-alt-boot",
                "-e",
                "efi/microsoft/boot/efisys.bin",
                "-no-emul-boot",
                "-boot-load-size",
                "2880",
                "-V",
                "TESTVOL",
                "-o",
                output.to_str().unwrap(),
                tree.to_str().unwrap(),
            ])
            .status()
            .map_err(|e| CoreError::new(e.to_string()))?;
        if !status.success() {
            return Err(CoreError::new("genisoimage falhou"));
        }
        Ok(())
    }

    fn build_without_udf(tree: &Path, output: &Path) -> CoreResult<()> {
        let xorriso = crate::core::process::locate_tool("xorriso")
            .ok_or_else(|| CoreError::new("xorriso ausente"))?;
        let status = Command::new(xorriso)
            .args([
                "-as",
                "mkisofs",
                "-iso-level",
                "4",
                "-D",
                "-N",
                "-b",
                "boot/etfsboot.com",
                "-no-emul-boot",
                "-boot-load-size",
                "8",
                "-eltorito-alt-boot",
                "-e",
                "efi/microsoft/boot/efisys.bin",
                "-no-emul-boot",
                "-boot-load-size",
                "2880",
                "-V",
                "TESTVOL",
                "-o",
                output.to_str().unwrap(),
                tree.to_str().unwrap(),
            ])
            .status()
            .map_err(|e| CoreError::new(e.to_string()))?;
        if !status.success() {
            return Err(CoreError::new("xorriso falhou"));
        }
        Ok(())
    }

    #[test]
    fn validates_a_hybrid_iso_and_detects_lost_udf() {
        if crate::core::process::locate_tool("genisoimage").is_none() {
            eprintln!("genisoimage ausente; teste ignorado");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        write_tree(&tree, true).unwrap();
        let original = dir.path().join("original.iso");
        build_hybrid(&tree, &original).unwrap();

        let original_meta = inspect(&original).unwrap();
        let rebuilt = dir.path().join("rebuilt.iso");
        build_hybrid(&tree, &rebuilt).unwrap();
        let rebuilt_meta = inspect(&rebuilt).unwrap();

        let report = validate(&original, &rebuilt, &original_meta, &rebuilt_meta, &tree).unwrap();
        assert_eq!(report.final_status, FinalStatus::StructurallyValid);
        assert!(!report.has_failures(), "{:#?}", report.checks);

        let no_udf = dir.path().join("no-udf.iso");
        build_without_udf(&tree, &no_udf).unwrap();
        let no_udf_meta = inspect(&no_udf).unwrap();
        let report = validate(&original, &no_udf, &original_meta, &no_udf_meta, &tree).unwrap();
        assert_eq!(report.final_status, FinalStatus::Invalid);
        assert!(report
            .checks
            .iter()
            .any(|check| { check.id == "fs:udf" && check.status == CheckStatus::Fail }));
    }

    #[test]
    fn detects_missing_bootmgr() {
        if crate::core::process::locate_tool("genisoimage").is_none() {
            eprintln!("genisoimage ausente; teste ignorado");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let original_tree = dir.path().join("original-tree");
        write_tree(&original_tree, true).unwrap();
        let original = dir.path().join("original.iso");
        build_hybrid(&original_tree, &original).unwrap();
        let original_meta = inspect(&original).unwrap();

        let broken_tree = dir.path().join("broken-tree");
        write_tree(&broken_tree, false).unwrap();
        let broken = dir.path().join("broken.iso");
        build_hybrid(&broken_tree, &broken).unwrap();
        let broken_meta = inspect(&broken).unwrap();

        let report = validate(
            &original,
            &broken,
            &original_meta,
            &broken_meta,
            &broken_tree,
        )
        .unwrap();
        assert_eq!(report.final_status, FinalStatus::Invalid);
        assert!(report
            .checks
            .iter()
            .any(|check| { check.id == "file:/bootmgr" && check.status == CheckStatus::Fail }));
    }
}
