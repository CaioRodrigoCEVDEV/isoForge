use crate::core::error::{CoreError, CoreResult};
use crate::core::fsutil::{ensure_writable_tree, iso_rr_to_relative, join_iso_path, scan_tree};
use crate::core::iso_inspector::{inspect, list_files, read_boot_image};
use crate::core::model::{BackendCapability, IsoMetadata, SupportLevel};
use crate::core::process::{available_space, locate_tool, run_polling, CancelToken};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub enum Progress {
    Stage(String),
    Bytes { done: u64, total: u64 },
}

pub struct BuildRequest<'a> {
    pub original: &'a Path,
    pub tree: &'a Path,
    pub metadata: &'a IsoMetadata,
    pub output: &'a Path,
}

#[derive(Clone, Debug)]
pub struct BuildResult {
    pub output: PathBuf,
    pub backend: String,
    pub actions: Vec<String>,
    pub warnings: Vec<String>,
}

pub trait IsoBackend {
    fn name(&self) -> &'static str;

    fn capability(&self, metadata: &IsoMetadata, tree: Option<&Path>) -> BackendCapability;

    fn rebuild(
        &self,
        request: &BuildRequest,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> CoreResult<BuildResult>;
}

pub fn select_backend(metadata: &IsoMetadata) -> Box<dyn IsoBackend> {
    if metadata.is_udf_only() {
        Box::new(UdfBackend)
    } else if metadata.is_hybrid_iso9660_udf() {
        Box::new(HybridIsoBackend)
    } else {
        Box::new(XorrisoBackend)
    }
}

pub fn estimate_output_size(metadata: &IsoMetadata, tree: Option<&Path>) -> u64 {
    let tree_bytes = tree.map(|path| scan_tree(path).1).unwrap_or(0);
    // In a hybrid ISO the ISO9660 and UDF layers can share data extents, as
    // verified with genisoimage 1.1.11. The output is therefore close to the
    // larger of the extracted tree and the original image.
    tree_bytes
        .max(metadata.size)
        .saturating_add(32 * 1024 * 1024)
}

fn common_blockers(metadata: &IsoMetadata) -> Vec<String> {
    let mut blockers = Vec::new();
    if !metadata.has_iso9660 {
        blockers.push(
            "A imagem não possui camada ISO9660; reconstrução segura não implementada.".to_string(),
        );
    }
    if metadata.has_system_area {
        blockers.push(format!(
            "Mídia híbrida ({}) detectada. Reconstrução com MBR/GPT ainda não é suportada com segurança.",
            metadata.system_area_information
        ));
    }
    for entry in &metadata.boot_entries {
        if !entry.is_bios() && !entry.is_uefi() {
            blockers.push(format!(
                "Entrada El Torito de plataforma '{}' não suportada; não será descartada silenciosamente.",
                entry.platform
            ));
        }
    }
    blockers
}

fn common_warnings(metadata: &IsoMetadata) -> Vec<String> {
    let mut warnings = Vec::new();
    if metadata.has_udf {
        warnings.push(
            "A camada UDF será recriada com genisoimage -udf -allow-limited-size.".to_string(),
        );
    }
    if metadata.has_windows_layout() {
        warnings.push(
            "Layout Windows detectado; a validação estrutural não substitui um teste de boot em VM."
                .to_string(),
        );
    }
    if !metadata.has_el_torito {
        warnings.push(
            "A ISO original não possui El Torito; a nova imagem não será bootável.".to_string(),
        );
    }
    warnings
}

struct HybridIsoBackend;
struct XorrisoBackend;
struct UdfBackend;

impl IsoBackend for HybridIsoBackend {
    fn name(&self) -> &'static str {
        "HybridIsoBackend (genisoimage UDF + El Torito)"
    }

    fn capability(&self, metadata: &IsoMetadata, tree: Option<&Path>) -> BackendCapability {
        let mut blockers = common_blockers(metadata);
        let warnings = common_warnings(metadata);

        if locate_tool("genisoimage").is_none() {
            blockers.push(
                "genisoimage não encontrado. Sem ele não é possível recriar UDF. \
                 Instale genisoimage (ou defina ISOFORGE_GENISOIMAGE)."
                    .to_string(),
            );
        }

        let bios = metadata.boot_entries_for("BIOS");
        let uefi = metadata.boot_entries_for("UEFI");
        if bios.len() > 1 {
            blockers.push("Mais de uma entrada El Torito BIOS não é suportada.".to_string());
        }
        if uefi.len() > 1 {
            blockers.push("Mais de uma entrada El Torito UEFI não é suportada.".to_string());
        }
        for entry in bios.iter().chain(uefi.iter()) {
            if entry.bootable && entry.image_path.is_none() {
                blockers.push(format!(
                    "Entrada El Torito {} sem caminho de imagem identificado.",
                    entry.platform
                ));
            }
        }

        BackendCapability {
            backend: self.name().to_string(),
            level: if blockers.is_empty() {
                SupportLevel::Full
            } else {
                SupportLevel::Unsupported
            },
            blockers,
            warnings,
            estimated_output_size: estimate_output_size(metadata, tree),
        }
    }

    fn rebuild(
        &self,
        request: &BuildRequest,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> CoreResult<BuildResult> {
        let capability = self.capability(request.metadata, Some(request.tree));
        if !capability.is_supported() {
            return Err(CoreError::new(format!(
                "Reconstrução não suportada: {}",
                capability.blockers.join(" | ")
            )));
        }
        let genisoimage = locate_tool("genisoimage").ok_or_else(|| {
            CoreError::new(
                "genisoimage não encontrado. Instale genisoimage ou defina ISOFORGE_GENISOIMAGE.",
            )
        })?;

        rebuild_with_mkisofs(
            &genisoimage,
            true,
            false,
            request,
            cancel,
            progress,
            self.name(),
            capability.warnings,
        )
    }
}

impl IsoBackend for XorrisoBackend {
    fn name(&self) -> &'static str {
        "XorrisoBackend (ISO9660 + El Torito, sem UDF)"
    }

    fn capability(&self, metadata: &IsoMetadata, tree: Option<&Path>) -> BackendCapability {
        let mut blockers = common_blockers(metadata);
        let warnings = common_warnings(metadata);

        if metadata.has_udf {
            blockers.push(
                "xorriso não escreve UDF; use HybridIsoBackend para imagens UDF.".to_string(),
            );
        }
        if locate_tool("xorriso").is_none() {
            blockers.push("xorriso não encontrado.".to_string());
        }
        let bios = metadata.boot_entries_for("BIOS");
        let uefi = metadata.boot_entries_for("UEFI");
        if bios.len() > 1 {
            blockers.push("Mais de uma entrada El Torito BIOS não é suportada.".to_string());
        }
        if uefi.len() > 1 {
            blockers.push("Mais de uma entrada El Torito UEFI não é suportada.".to_string());
        }
        for entry in bios.iter().chain(uefi.iter()) {
            if entry.bootable && entry.image_path.is_none() {
                blockers.push(format!(
                    "Entrada El Torito {} sem caminho de imagem identificado.",
                    entry.platform
                ));
            }
        }

        BackendCapability {
            backend: self.name().to_string(),
            level: if blockers.is_empty() {
                SupportLevel::Full
            } else {
                SupportLevel::Unsupported
            },
            blockers,
            warnings,
            estimated_output_size: estimate_output_size(metadata, tree),
        }
    }

    fn rebuild(
        &self,
        request: &BuildRequest,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(Progress),
    ) -> CoreResult<BuildResult> {
        let capability = self.capability(request.metadata, Some(request.tree));
        if !capability.is_supported() {
            return Err(CoreError::new(format!(
                "Reconstrução não suportada: {}",
                capability.blockers.join(" | ")
            )));
        }
        let xorriso =
            locate_tool("xorriso").ok_or_else(|| CoreError::new("xorriso não encontrado."))?;

        rebuild_with_mkisofs(
            &xorriso,
            false,
            true,
            request,
            cancel,
            progress,
            self.name(),
            capability.warnings,
        )
    }
}

impl IsoBackend for UdfBackend {
    fn name(&self) -> &'static str {
        "UdfBackend (somente leitura)"
    }

    fn capability(&self, metadata: &IsoMetadata, tree: Option<&Path>) -> BackendCapability {
        BackendCapability {
            backend: self.name().to_string(),
            level: SupportLevel::ReadOnly,
            blockers: vec![
                "Imagem UDF sem camada ISO9660: a reconstrução segura de UDF puro não está implementada. \
                 A imagem pode ser aberta somente para leitura."
                    .to_string(),
            ],
            warnings: Vec::new(),
            estimated_output_size: estimate_output_size(metadata, tree),
        }
    }

    fn rebuild(
        &self,
        _request: &BuildRequest,
        _cancel: &CancelToken,
        _progress: &mut dyn FnMut(Progress),
    ) -> CoreResult<BuildResult> {
        Err(CoreError::new(
            "Reconstrução de UDF puro não é suportada. Nenhum arquivo foi gerado.",
        ))
    }
}

fn rebuild_with_mkisofs(
    tool: &Path,
    include_udf: bool,
    xorriso_mode: bool,
    request: &BuildRequest,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(Progress),
    backend: &str,
    mut warnings: Vec<String>,
) -> CoreResult<BuildResult> {
    let output = request.output;
    if let Ok(metadata) = fs::metadata(output) {
        if metadata.is_file() {
            return Err(CoreError::new(format!(
                "O arquivo de destino já existe: '{}'. Escolha outro nome.",
                output.display()
            )));
        }
    }
    if same_file(request.original, output) {
        return Err(CoreError::new(
            "O destino não pode ser a ISO original. Escolha outro caminho.",
        ));
    }

    let total_size = estimate_output_size(request.metadata, Some(request.tree));
    let parent = output
        .parent()
        .ok_or_else(|| CoreError::new("Diretório de saída inválido."))?;
    let free = available_space(parent)?;
    let required = total_size.saturating_add(512 * 1024 * 1024);
    if free < required {
        return Err(CoreError::new(format!(
            "Espaço insuficiente em '{}'. Necessário ~{} GB, disponível ~{} GB.",
            parent.display(),
            required / 1_000_000_000,
            free / 1_000_000_000
        )));
    }

    progress(Progress::Stage("Preparando imagens de boot...".to_string()));
    let (actions, boot_warnings) = prepare_boot_images(request, progress)?;
    warnings.extend(boot_warnings);

    let temp_output = temporary_output_path(output);
    let mut args = build_mkisofs_args(request.metadata, request.tree, &temp_output, include_udf)?;
    if xorriso_mode {
        args.splice(0..0, ["-as".to_string(), "mkisofs".to_string()]);
    }

    progress(Progress::Stage("Reconstruindo ISO...".to_string()));
    let result = run_polling(
        tool,
        &args,
        cancel,
        || {
            fs::metadata(&temp_output)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        },
        |done| {
            progress(Progress::Bytes {
                done,
                total: total_size,
            })
        },
    );

    if let Err(err) = result {
        let _ = fs::remove_file(&temp_output);
        return Err(err);
    }
    let result = result.expect("resultado já verificado");
    if !result.success {
        let _ = fs::remove_file(&temp_output);
        return Err(CoreError::new(format!(
            "A reconstrução falhou ({}) ao executar '{}':\n{}",
            result
                .status
                .map(|code| format!("código {code}"))
                .unwrap_or_else(|| "sem código de saída".to_string()),
            result.command_line(),
            tail(&result.log, 4000)
        )));
    }

    progress(Progress::Stage(
        "Validando estrutura da ISO gerada...".to_string(),
    ));
    let produced = match inspect(&temp_output) {
        Ok(metadata) => metadata,
        Err(err) => {
            let _ = fs::remove_file(&temp_output);
            return Err(CoreError::new(format!(
                "A ISO gerada não pôde ser inspecionada: {err}"
            )));
        }
    };
    let (structure_errors, structure_warnings) =
        compare_structure(request.original, &temp_output, request.metadata, &produced)?;
    warnings.extend(structure_warnings);
    if !structure_errors.is_empty() {
        let _ = fs::remove_file(&temp_output);
        return Err(CoreError::new(format!(
            "A ISO gerada foi descartada porque não preservou estruturas obrigatórias:\n- {}",
            structure_errors.join("\n- ")
        )));
    }

    if output.exists() {
        let _ = fs::remove_file(&temp_output);
        return Err(CoreError::new(format!(
            "O arquivo de destino passou a existir durante a reconstrução: '{}'.",
            output.display()
        )));
    }
    fs::rename(&temp_output, output).map_err(|e| {
        let _ = fs::remove_file(&temp_output);
        CoreError::new(format!(
            "Não foi possível mover a ISO final para '{}': {e}",
            output.display()
        ))
    })?;
    let _ = ensure_writable_tree(request.tree);

    Ok(BuildResult {
        output: output.to_path_buf(),
        backend: backend.to_string(),
        actions,
        warnings,
    })
}

fn build_mkisofs_args(
    metadata: &IsoMetadata,
    tree: &Path,
    output: &Path,
    include_udf: bool,
) -> CoreResult<Vec<String>> {
    let mut args = Vec::new();

    if include_udf {
        if !metadata.has_udf {
            return Err(CoreError::new(
                "Backend UDF selecionado, mas a ISO original não possui UDF.",
            ));
        }
        args.push("-udf".to_string());
        args.push("-allow-limited-size".to_string());
    } else if metadata.has_udf {
        return Err(CoreError::new(
            "Este backend não pode recriar UDF; use HybridIsoBackend.",
        ));
    }

    if metadata.iso9660_1999 || metadata.has_windows_layout() {
        args.extend(["-iso-level", "4", "-D", "-N"].map(str::to_string));
    } else {
        args.extend(["-iso-level", "3"].map(str::to_string));
    }

    if metadata.has_joliet {
        args.push("-J".to_string());
        args.push("-joliet-long".to_string());
    }
    if metadata.has_rock_ridge {
        args.push("-R".to_string());
    }
    if let Some(volume_id) = &metadata.volume_id {
        args.push("-V".to_string());
        args.push(volume_id.clone());
    }

    let catalog = metadata.boot_catalog.as_deref().unwrap_or("/boot.catalog");
    if metadata.has_el_torito || !metadata.boot_entries.is_empty() {
        args.push("-c".to_string());
        args.push(iso_rr_to_relative(catalog));
    }

    let mut first_boot_added = false;
    for entry in metadata
        .boot_entries
        .iter()
        .filter(|entry| entry.bootable && (entry.is_bios() || entry.is_uefi()))
    {
        let path = entry.image_path.as_deref().ok_or_else(|| {
            CoreError::new(format!(
                "Entrada El Torito {} sem caminho de imagem.",
                entry.platform
            ))
        })?;
        let relative = iso_rr_to_relative(path);

        if entry.is_bios() {
            if first_boot_added {
                args.push("-eltorito-alt-boot".to_string());
            }
            args.push("-b".to_string());
            args.push(relative);
            args.push("-no-emul-boot".to_string());
            args.push("-boot-load-size".to_string());
            args.push(entry.load_size.to_string());
        } else {
            if first_boot_added {
                args.push("-eltorito-alt-boot".to_string());
            }
            args.push("-e".to_string());
            args.push(relative);
            args.push("-no-emul-boot".to_string());
            args.push("-boot-load-size".to_string());
            args.push(entry.load_size.to_string());
        }
        first_boot_added = true;
    }

    args.push("-o".to_string());
    args.push(output.display().to_string());
    args.push(tree.display().to_string());
    Ok(args)
}

fn prepare_boot_images(
    request: &BuildRequest,
    progress: &mut dyn FnMut(Progress),
) -> CoreResult<(Vec<String>, Vec<String>)> {
    let mut actions = Vec::new();
    let mut warnings = Vec::new();

    for entry in request
        .metadata
        .boot_entries
        .iter()
        .filter(|entry| entry.bootable && (entry.is_bios() || entry.is_uefi()))
    {
        let Some(image_path) = entry.image_path.as_deref() else {
            return Err(CoreError::new(format!(
                "Entrada El Torito {} não possui caminho de imagem; recusando reconstruir.",
                entry.platform
            )));
        };

        let boot_bytes = read_boot_image(request.original, entry)?;
        if boot_bytes.is_empty() {
            return Err(CoreError::new(format!(
                "Imagem El Torito {} vazia.",
                entry.platform
            )));
        }

        let destination = join_iso_path(request.tree, image_path);
        let needs_write = match fs::read(&destination) {
            Ok(existing) => {
                let prefix = existing.len().min(boot_bytes.len());
                existing.len() < boot_bytes.len() || existing[..prefix] != boot_bytes[..prefix]
            }
            Err(_) => true,
        };

        if needs_write {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&destination, &boot_bytes).map_err(|e| {
                CoreError::new(format!(
                    "Não foi possível gravar a imagem de boot '{}': {e}",
                    destination.display()
                ))
            })?;
            actions.push(format!(
                "Imagem de boot {} restaurada do LBA {} para '{}' ({} bytes).",
                entry.platform,
                entry.lba,
                image_path,
                boot_bytes.len()
            ));
        } else if fs::metadata(&destination).map(|m| m.len()).unwrap_or(0)
            != boot_bytes.len() as u64
        {
            warnings.push(format!(
                "A imagem de boot '{}' é maior que a carga El Torito ({} bytes); conteúdo extra preservado.",
                image_path,
                boot_bytes.len()
            ));
        }

        let _ = progress;
    }

    Ok((actions, warnings))
}

fn compare_structure(
    original: &Path,
    produced: &Path,
    expected: &IsoMetadata,
    actual: &IsoMetadata,
) -> CoreResult<(Vec<String>, Vec<String>)> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if expected.has_iso9660 && !actual.has_iso9660 {
        errors.push("Camada ISO9660 ausente na ISO gerada.".to_string());
    }
    if expected.has_udf && !actual.has_udf {
        errors.push("Camada UDF ausente na ISO gerada.".to_string());
    }
    if expected.has_joliet && !actual.has_joliet {
        warnings.push(
            "Joliet estava presente na original e não foi detectado na nova ISO.".to_string(),
        );
    }
    if expected.has_rock_ridge && !actual.has_rock_ridge {
        warnings.push(
            "Rock Ridge estava presente na original e não foi detectado na nova ISO.".to_string(),
        );
    }
    if let (Some(expected_version), Some(actual_version)) =
        (&expected.udf_version, &actual.udf_version)
    {
        if expected_version != actual_version {
            warnings.push(format!(
                "Versão UDF mudou de {expected_version} para {actual_version}."
            ));
        }
    }
    if expected.has_el_torito && !actual.has_el_torito {
        errors.push("El Torito ausente na ISO gerada.".to_string());
    }
    if expected.has_system_area && !actual.has_system_area {
        errors.push("Área de sistema MBR/GPT ausente na ISO gerada.".to_string());
    }

    for path in ["/sources/install.wim", "/sources/install.esd"] {
        let expected_file = expected
            .essential_files
            .iter()
            .find(|file| file.path.eq_ignore_ascii_case(path) && file.present);
        let actual_file = actual
            .essential_files
            .iter()
            .find(|file| file.path.eq_ignore_ascii_case(path) && file.present);
        match (expected_file, actual_file) {
            (Some(expected_file), Some(actual_file)) => {
                if expected_file.size != actual_file.size {
                    errors.push(format!(
                        "{} mudou de {:?} para {:?} bytes (conteúdo UDF pode ter sido truncado).",
                        path, expected_file.size, actual_file.size
                    ));
                }
            }
            (Some(_), None) => {
                errors.push(format!("{path} ausente na ISO gerada."));
            }
            _ => {}
        }
    }

    for expected_entry in expected
        .boot_entries
        .iter()
        .filter(|entry| entry.bootable && (entry.is_bios() || entry.is_uefi()))
    {
        let Some(actual_entry) = actual.boot_entries.iter().find(|candidate| {
            candidate.is_bios() == expected_entry.is_bios()
                && candidate.is_uefi() == expected_entry.is_uefi()
                && candidate
                    .image_path
                    .as_deref()
                    .map(|path| path.to_ascii_lowercase())
                    == expected_entry
                        .image_path
                        .as_deref()
                        .map(|path| path.to_ascii_lowercase())
        }) else {
            errors.push(format!(
                "Entrada El Torito {} ({}) ausente na ISO gerada.",
                expected_entry.platform,
                expected_entry
                    .image_path
                    .as_deref()
                    .unwrap_or("sem caminho")
            ));
            continue;
        };

        if actual_entry.load_size != expected_entry.load_size {
            errors.push(format!(
                "Entrada El Torito {} mudou o tamanho de carga de {} para {} setores.",
                expected_entry.platform, expected_entry.load_size, actual_entry.load_size
            ));
        }

        let original_bytes = read_boot_image(original, expected_entry)?;
        let produced_bytes = read_boot_image(produced, actual_entry)?;
        if original_bytes != produced_bytes {
            errors.push(format!(
                "Imagem de boot {} não é idêntica à original ({} vs {} bytes).",
                expected_entry.platform,
                original_bytes.len(),
                produced_bytes.len()
            ));
        }
    }

    Ok((errors, warnings))
}

fn tail(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        text[text.len() - max_chars..].to_string()
    }
}

fn temporary_output_path(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "isoforge.iso".to_string());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        ".{name}.isoforge-{}-{stamp}.tmp",
        std::process::id()
    ))
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn extract_iso(
    iso: &Path,
    metadata: &IsoMetadata,
    destination: &Path,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(Progress),
) -> CoreResult<()> {
    let result = extract_iso_inner(iso, metadata, destination, cancel, progress);
    // Even a cancelled/failed extraction may have created read-only files;
    // make them writable so the TempDir can be cleaned up.
    let _ = ensure_writable_tree(destination);
    result
}

fn extract_iso_inner(
    iso: &Path,
    metadata: &IsoMetadata,
    destination: &Path,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(Progress),
) -> CoreResult<()> {
    fs::create_dir_all(destination)?;

    if metadata.has_udf {
        // A UDF layer is authoritative. Using the ISO9660 view of a hybrid
        // Windows image can silently extract a truncated/limited-size WIM.
        let seven_zip = locate_tool("7z").ok_or_else(|| {
            CoreError::new(
                "7z é obrigatório para extrair a camada UDF com segurança. \
                 Instale p7zip-full ou defina ISOFORGE_7Z.",
            )
        })?;
        let files = list_files(iso, metadata).unwrap_or_default();
        let total = files
            .iter()
            .fold(0u64, |sum, file| sum.saturating_add(file.size));
        let total = if total > 0 { total } else { metadata.size };

        progress(Progress::Stage(
            "Extraindo conteúdo UDF (fonte de verdade)...".to_string(),
        ));
        let args = vec![
            "x".to_string(),
            "-y".to_string(),
            "-tUdf".to_string(),
            format!("-o{}", destination.display()),
            iso.display().to_string(),
        ];
        let result = run_polling(
            &seven_zip,
            &args,
            cancel,
            || scan_tree(destination).1,
            |done| progress(Progress::Bytes { done, total }),
        )?;
        if !result.success {
            return Err(CoreError::new(format!(
                "7z falhou ao extrair a camada UDF:\n{}",
                tail(&result.log, 4000)
            )));
        }
    } else if metadata.has_iso9660 {
        let xorriso =
            locate_tool("xorriso").ok_or_else(|| CoreError::new("xorriso não encontrado."))?;
        let files = list_files(iso, metadata).unwrap_or_default();
        let total = files
            .iter()
            .fold(0u64, |sum, file| sum.saturating_add(file.size));
        let total = if total > 0 { total } else { metadata.size };

        progress(Progress::Stage("Extraindo conteúdo ISO9660...".to_string()));
        let args = vec![
            "-osirrox".to_string(),
            "on:auto_chmod_on".to_string(),
            "-indev".to_string(),
            iso.display().to_string(),
            "-extract".to_string(),
            "/".to_string(),
            destination.display().to_string(),
        ];
        let result = run_polling(
            &xorriso,
            &args,
            cancel,
            || scan_tree(destination).1,
            |done| progress(Progress::Bytes { done, total }),
        )?;
        if !result.success {
            return Err(CoreError::new(format!(
                "xorriso falhou ao extrair a ISO:\n{}",
                tail(&result.log, 4000)
            )));
        }
    } else {
        return Err(CoreError::new(
            "A imagem não possui ISO9660 nem UDF reconhecíveis.",
        ));
    }

    ensure_writable_tree(destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::iso_inspector::inspect;
    use std::process::Command;

    fn genisoimage() -> Option<PathBuf> {
        locate_tool("genisoimage")
    }

    fn xorriso() -> Option<PathBuf> {
        locate_tool("xorriso")
    }

    fn write_synthetic_boot_tree(root: &Path) -> CoreResult<()> {
        fs::create_dir_all(root.join("boot"))?;
        fs::create_dir_all(root.join("efi/microsoft/boot"))?;
        fs::create_dir_all(root.join("efi/boot"))?;
        fs::create_dir_all(root.join("sources"))?;
        fs::write(root.join("boot/etfsboot.com"), vec![0x90u8; 4096])?;
        fs::write(
            root.join("efi/microsoft/boot/efisys.bin"),
            vec![0x42u8; 1474560],
        )?;
        fs::write(root.join("bootmgr"), b"bootmgr")?;
        fs::write(root.join("efi/boot/bootx64.efi"), b"bootx64")?;
        fs::write(root.join("sources/install.wim"), vec![0x5Au8; 100_000])?;
        fs::write(root.join("autounattend.xml"), b"<unattend/>")?;
        Ok(())
    }

    #[test]
    fn hybrid_backend_preserves_udf_and_boot_images() {
        let Some(geniso) = genisoimage() else {
            eprintln!("genisoimage ausente; teste ignorado");
            return;
        };

        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("tree");
        write_synthetic_boot_tree(&tree).unwrap();
        let original = source.path().join("original.iso");
        let status = Command::new(&geniso)
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
                original.to_str().unwrap(),
                tree.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let metadata = inspect(&original).unwrap();
        assert!(metadata.has_iso9660);
        assert!(metadata.has_udf);
        assert!(metadata.has_bios_boot);
        assert!(metadata.has_uefi_boot);

        fs::write(tree.join("autounattend.xml"), b"<unattend>new</unattend>").unwrap();
        fs::write(tree.join("novo.txt"), b"novo").unwrap();

        let output = source.path().join("rebuilt.iso");
        let backend = HybridIsoBackend;
        let request = BuildRequest {
            original: &original,
            tree: &tree,
            metadata: &metadata,
            output: &output,
        };
        let cancel = CancelToken::new();
        backend
            .rebuild(&request, &cancel, &mut |_| {})
            .expect("reconstrução deve funcionar");

        let rebuilt = inspect(&output).unwrap();
        assert!(rebuilt.has_udf, "UDF deve ser preservado");
        assert!(rebuilt.has_bios_boot);
        assert!(rebuilt.has_uefi_boot);
        assert!(rebuilt
            .essential_files
            .iter()
            .any(|file| { file.path == "/autounattend.xml" && file.present }));
    }

    #[test]
    fn xorriso_backend_builds_without_udf() {
        let Some(xorriso) = xorriso() else {
            eprintln!("xorriso ausente; teste ignorado");
            return;
        };
        let source = tempfile::tempdir().unwrap();
        let tree = source.path().join("tree");
        write_synthetic_boot_tree(&tree).unwrap();
        let original = source.path().join("original.iso");
        // A small non-UDF bootable ISO built by xorriso.
        let status = Command::new(&xorriso)
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
                original.to_str().unwrap(),
                tree.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let metadata = inspect(&original).unwrap();
        assert!(!metadata.has_udf);
        let output = source.path().join("rebuilt.iso");
        let backend = XorrisoBackend;
        let request = BuildRequest {
            original: &original,
            tree: &tree,
            metadata: &metadata,
            output: &output,
        };
        backend
            .rebuild(&request, &CancelToken::new(), &mut |_| {})
            .expect("reconstrução xorriso deve funcionar");
        assert!(output.is_file());
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

    /// Full round trip against the real Windows ISO. This test is intentionally
    /// ignored because it reads/creates several gigabytes and takes minutes.
    ///
    /// Run with:
    ///   ISOFORGE_REAL_ISO=/path/to/original.iso cargo test -- --ignored real_iso_roundtrip --nocapture
    #[test]
    #[ignore]
    fn real_iso_roundtrip() {
        let Ok(original_path) = std::env::var("ISOFORGE_REAL_ISO") else {
            eprintln!("ISOFORGE_REAL_ISO não definido; teste ignorado");
            return;
        };
        let original = PathBuf::from(original_path);
        let metadata = inspect(&original).expect("inspeção da ISO real");
        eprintln!(
            "ISO real: {} ({}, {}, boot {})",
            original.display(),
            human_size(metadata.size),
            metadata.filesystem_summary(),
            metadata.boot_summary()
        );

        let workspace = crate::core::process::create_workspace().expect("workspace");
        let tree = workspace.path().join("contents");
        fs::create_dir_all(&tree).unwrap();

        let cancel = CancelToken::new();
        let mut last_percent = 0u64;
        extract_iso(&original, &metadata, &tree, &cancel, &mut |progress| {
            if let Progress::Bytes { done, total } = progress {
                if total > 0 {
                    let percent = done.saturating_mul(100) / total;
                    if percent >= last_percent + 10 {
                        last_percent = percent;
                        eprintln!("extração: {percent}%");
                    }
                }
            }
        })
        .expect("extração da ISO real");
        eprintln!(
            "workspace extraído: {} ({})",
            tree.display(),
            human_size(scan_tree(&tree).1)
        );

        let auto_unattend = original
            .parent()
            .map(|parent| parent.join("autounattend.xml"))
            .filter(|path| path.is_file());
        if let Some(path) = &auto_unattend {
            fs::copy(path, tree.join("autounattend.xml")).unwrap();
            eprintln!("autounattend.xml copiado de {}", path.display());
        }

        let backend = HybridIsoBackend;
        let capability = backend.capability(&metadata, Some(&tree));
        assert!(
            capability.is_supported(),
            "backend indisponível: {:?}",
            capability.blockers
        );

        let output_dir = tempfile::tempdir_in(crate::core::process::workspace_root().unwrap())
            .expect("diretório de saída");
        let output = output_dir.path().join("isoforge-real-roundtrip.iso");
        let request = BuildRequest {
            original: &original,
            tree: &tree,
            metadata: &metadata,
            output: &output,
        };

        let mut last_percent = 0u64;
        backend
            .rebuild(&request, &cancel, &mut |progress| {
                if let Progress::Bytes { done, total } = progress {
                    if total > 0 {
                        let percent = done.saturating_mul(100) / total;
                        if percent >= last_percent + 10 {
                            last_percent = percent;
                            eprintln!("reconstrução: {percent}%");
                        }
                    }
                }
            })
            .expect("reconstrução da ISO real");

        let rebuilt_meta = inspect(&output).expect("inspeção da ISO gerada");
        let report = crate::core::iso_validator::validate(
            &original,
            &output,
            &metadata,
            &rebuilt_meta,
            &tree,
        )
        .expect("validação da ISO gerada");

        for check in &report.checks {
            eprintln!(
                "{:<8} {:<40} {}",
                check.status.label(),
                check.label,
                check.detail
            );
        }
        eprintln!("status final: {:?}", report.final_status);
        eprintln!(
            "tamanho original: {} | gerado: {}",
            human_size(metadata.size),
            human_size(rebuilt_meta.size)
        );

        assert_eq!(
            report.final_status,
            crate::core::iso_validator::FinalStatus::StructurallyValid
        );
        assert!(!report.has_failures(), "relatório contém FAIL: {report:#?}");
        assert!(rebuilt_meta.has_udf, "UDF não preservado");
        assert!(rebuilt_meta.has_bios_boot, "boot BIOS não preservado");
        assert!(rebuilt_meta.has_uefi_boot, "boot UEFI não preservado");

        let original_udf =
            crate::core::iso_inspector::list_files_udf(&original).expect("listagem UDF original");
        let rebuilt_udf =
            crate::core::iso_inspector::list_files_udf(&output).expect("listagem UDF gerada");
        for path in ["/sources/install.wim", "/sources/install.esd"] {
            let original_file = original_udf
                .iter()
                .find(|file| file.path.eq_ignore_ascii_case(path));
            let rebuilt_file = rebuilt_udf
                .iter()
                .find(|file| file.path.eq_ignore_ascii_case(path));
            if let (Some(original_file), Some(rebuilt_file)) = (original_file, rebuilt_file) {
                eprintln!(
                    "{} UDF: original {} bytes | gerado {} bytes",
                    path, original_file.size, rebuilt_file.size
                );
                assert_eq!(
                    original_file.size, rebuilt_file.size,
                    "conteúdo UDF de {} não preservado",
                    path
                );
            }
        }
        assert!(
            rebuilt_udf
                .iter()
                .any(|file| file.path.eq_ignore_ascii_case("/autounattend.xml")),
            "autounattend.xml ausente na camada UDF"
        );
        // The overall size may differ because ISO9660 and UDF can share data
        // extents. The meaningful check is the UDF file sizes above.
        eprintln!(
            "tamanhos: original {} | gerado {}",
            human_size(metadata.size),
            human_size(rebuilt_meta.size)
        );
    }
}
