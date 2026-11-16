use crate::core::error::{CoreError, CoreResult};
use crate::core::model::{essential_files_from, BootEntry, IsoFile, IsoMetadata};
use crate::core::process::{locate_tool, run};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SECTOR: u64 = 2048;

fn tool_or_error(name: &str) -> CoreResult<std::path::PathBuf> {
    locate_tool(name).ok_or_else(|| {
        CoreError::new(format!(
            "Ferramenta obrigatória '{name}' não encontrada. Instale-a ou defina ISOFORGE_{}.",
            name.to_ascii_uppercase().replace('-', "_")
        ))
    })
}

fn read_at(path: &Path, offset: u64, len: usize) -> CoreResult<Vec<u8>> {
    let mut file = File::open(path)
        .map_err(|e| CoreError::new(format!("Não foi possível abrir '{}': {e}", path.display())))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0u8; len];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn read_optional(path: &Path, offset: u64, len: usize) -> Option<Vec<u8>> {
    read_at(path, offset, len).ok()
}

fn le16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn le32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn ascii_field(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .trim_matches(|c: char| c == '\0' || c == ' ')
        .to_string()
}

#[derive(Clone, Debug)]
struct Pvd {
    volume_id: String,
    system_id: String,
    version: u8,
    root_extent_lba: u64,
    root_extent_len: u64,
}

fn read_pvd(iso: &Path) -> CoreResult<Pvd> {
    let data = read_at(iso, 16 * SECTOR, SECTOR as usize)?;
    if &data[1..6] != b"CD001" {
        return Err(CoreError::new(
            "Descritor primário ISO9660 (CD001) não encontrado.",
        ));
    }

    let root = &data[156..190];
    Ok(Pvd {
        volume_id: ascii_field(&data[40..72]),
        system_id: ascii_field(&data[8..40]),
        version: data[6],
        root_extent_lba: le32(root, 2) as u64,
        root_extent_len: le32(root, 10) as u64,
    })
}

fn detect_joliet(iso: &Path) -> bool {
    for sector in 17u64..=31 {
        let Some(data) = read_optional(iso, sector * SECTOR, SECTOR as usize) else {
            break;
        };
        if &data[1..6] != b"CD001" || data[0] != 2 {
            continue;
        }
        let escape = &data[88..91];
        if escape == b"%/@" || escape == b"%/C" || escape == b"%/E" {
            return true;
        }
    }
    false
}

fn detect_rock_ridge(iso: &Path, pvd: &Pvd) -> bool {
    if pvd.root_extent_len == 0 {
        return false;
    }
    let len = pvd.root_extent_len.min(256 * 1024) as usize;
    let Some(data) = read_optional(iso, pvd.root_extent_lba * SECTOR, len) else {
        return false;
    };

    // Rock Ridge is identified by the "SP" entry with the BE EF signature.
    data.windows(7).any(|w| w == b"SP\x07\x01\xbe\xef")
}

fn detect_udf(iso: &Path) -> Option<String> {
    // The UDF Volume Recognition Sequence starts at sector 16 and contains
    // BEA01 and NSR02 (UDF 1.02) or NSR03 (UDF 2.xx).
    let data = read_optional(iso, 16 * SECTOR, 16 * SECTOR as usize)?;
    let mut nsr = None;

    for chunk in data.chunks_exact(SECTOR as usize) {
        if chunk.len() < 6 {
            continue;
        }
        if &chunk[1..6] == b"NSR02" {
            nsr = Some("1.02".to_string());
        } else if &chunk[1..6] == b"NSR03" {
            nsr = Some("2.xx".to_string());
        }
    }

    // The anchor volume descriptor pointer at sector 256 is a good sanity
    // check for UDF.
    if let Some(anchor) = read_optional(iso, 256 * SECTOR, 16) {
        if &anchor[0..4] == b"AVDP" && nsr.is_none() {
            nsr = Some("desconhecida".to_string());
        }
    }

    nsr
}

fn detect_system_area(iso: &Path) -> (bool, bool, String) {
    let Some(area) = read_optional(iso, 0, 32 * SECTOR as usize) else {
        return (
            false,
            false,
            "Não foi possível ler a área de sistema.".into(),
        );
    };

    let has_mbr_signature = area.len() >= 512 && area[510] == 0x55 && area[511] == 0xAA;
    let has_partition_entries = area
        .get(446..510)
        .map(|entries| entries.iter().any(|byte| *byte != 0))
        .unwrap_or(false);
    let has_mbr = has_mbr_signature && has_partition_entries;
    let has_gpt = area.len() >= 2 * SECTOR as usize
        && &area[SECTOR as usize..SECTOR as usize + 8] == b"EFI PART";

    let information = match (has_mbr, has_gpt) {
        (true, true) => "MBR + GPT (mídia híbrida detectada)".to_string(),
        (true, false) => "MBR (mídia híbrida detectada)".to_string(),
        (false, true) => "GPT (mídia híbrida detectada)".to_string(),
        (false, false) => "Sem área de sistema MBR/GPT".to_string(),
    };

    (has_mbr, has_gpt, information)
}

#[derive(Default)]
struct ElToritoData {
    has_el_torito: bool,
    catalog_path: Option<String>,
    catalog_lba: Option<u64>,
    entries: Vec<BootEntry>,
}

fn parse_el_torito_report(text: &str) -> Option<ElToritoData> {
    let mut data = ElToritoData::default();

    for line in text.lines() {
        let line = line.trim();
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim();
        let value = value.trim();

        match label {
            "El Torito catalog" => {
                let mut parts = value.split_whitespace();
                data.catalog_lba = parts.next().and_then(|v| v.parse::<u64>().ok());
                data.has_el_torito = true;
            }
            "El Torito cat path" => {
                data.catalog_path = Some(value.trim_matches('\'').to_string());
            }
            "El Torito boot img" => {
                let fields: Vec<&str> = value.split_whitespace().collect();
                if fields.len() < 8 {
                    continue;
                }
                let Ok(index) = fields[0].parse::<u32>() else {
                    continue;
                };
                let Ok(load_size) = fields[6].parse::<u32>() else {
                    continue;
                };
                let Ok(lba) = fields[7].parse::<u64>() else {
                    continue;
                };
                data.entries.push(BootEntry {
                    index,
                    platform: fields[1].to_string(),
                    bootable: fields[2].eq_ignore_ascii_case("y"),
                    emulation: fields[3].to_string(),
                    load_segment: fields[4].to_string(),
                    header_points: fields[5].to_string(),
                    load_size,
                    lba,
                    image_path: None,
                });
                data.has_el_torito = true;
            }
            "El Torito img path" => {
                let Some((index, path)) = value.split_once(char::is_whitespace) else {
                    continue;
                };
                let Ok(index) = index.parse::<u32>() else {
                    continue;
                };
                let path = path.trim().trim_matches('\'').to_string();
                if let Some(entry) = data.entries.iter_mut().find(|entry| entry.index == index) {
                    entry.image_path = Some(path);
                }
            }
            _ => {}
        }
    }

    if data.has_el_torito {
        Some(data)
    } else {
        None
    }
}

#[derive(Default)]
struct BootRecord {
    has_el_torito: bool,
    catalog_lba: Option<u64>,
}

fn read_boot_record(iso: &Path) -> BootRecord {
    let mut record = BootRecord::default();
    let Some(data) = read_optional(iso, 17 * SECTOR, SECTOR as usize) else {
        return record;
    };
    if &data[1..6] != b"CD001" {
        return record;
    }
    let system_id = ascii_field(&data[7..39]);
    if system_id.contains("EL TORITO") {
        record.has_el_torito = true;
        record.catalog_lba = Some(le32(&data, 71) as u64);
    }
    record
}

fn parse_direct_el_torito(iso: &Path, pvd: &Pvd) -> Option<ElToritoData> {
    let record = read_boot_record(iso);
    if !record.has_el_torito {
        return None;
    }
    let catalog_lba = record.catalog_lba?;
    let catalog = read_optional(iso, catalog_lba * SECTOR, SECTOR as usize)?;

    let mut data = ElToritoData {
        has_el_torito: true,
        catalog_lba: Some(catalog_lba),
        catalog_path: Some("/boot.catalog".to_string()),
        ..Default::default()
    };

    if catalog.len() < 64 {
        return Some(data);
    }

    let validation = &catalog[0..32];
    let mut platform = match validation[1] {
        0xEF => "UEFI",
        _ => "BIOS",
    };

    let mut offset = 32usize;
    let mut index = 1u32;

    while offset + 32 <= catalog.len() && index <= 64 {
        let entry = &catalog[offset..offset + 32];
        let header_id = entry[0];

        if header_id == 0x90 || header_id == 0x91 {
            platform = match entry[1] {
                0xEF => "UEFI",
                _ => "BIOS",
            };
            offset += 32;
            continue;
        }

        if header_id != 0x88 && header_id != 0x00 {
            break;
        }

        let load_size = le16(entry, 6) as u32;
        let lba = le32(entry, 8) as u64;
        let bootable = header_id == 0x88;
        let emulation = match entry[1] {
            0 => "none",
            1 => "1.2M",
            2 => "1.44M",
            3 => "2.88M",
            4 => "hd",
            _ => "unknown",
        };

        let mut visited = HashSet::new();
        let image_path = find_iso_path_by_lba(
            iso,
            pvd,
            pvd.root_extent_lba,
            pvd.root_extent_len,
            lba,
            load_size.max(1) as u64 * 512,
            0,
            &mut visited,
        );

        data.entries.push(BootEntry {
            index,
            platform: platform.to_string(),
            bootable,
            emulation: emulation.to_string(),
            load_segment: format!("0x{:04X}", le16(entry, 2)),
            header_points: format!("0x{:02X}", entry[4]),
            load_size,
            lba,
            image_path,
        });

        index += 1;
        offset += 32;
    }

    Some(data)
}

#[allow(clippy::too_many_arguments)]
fn find_iso_path_by_lba(
    iso: &Path,
    _pvd: &Pvd,
    dir_lba: u64,
    dir_len: u64,
    target_lba: u64,
    target_size: u64,
    depth: usize,
    visited: &mut HashSet<u64>,
) -> Option<String> {
    if depth > 32 || !visited.insert(dir_lba) {
        return None;
    }

    let len = dir_len.min(16 * 1024 * 1024) as usize;
    let data = read_optional(iso, dir_lba * SECTOR, len)?;
    let mut offset = 0usize;

    while offset < data.len() {
        let record_len = data[offset] as usize;
        if record_len == 0 {
            offset = ((offset / SECTOR as usize) + 1) * SECTOR as usize;
            continue;
        }
        if offset + record_len > data.len() || record_len < 34 {
            break;
        }

        let record = &data[offset..offset + record_len];
        let extent = le32(record, 2) as u64;
        let size = le32(record, 10) as u64;
        let flags = record[25];
        let name_len = record[32] as usize;
        if 33 + name_len > record.len() {
            offset += record_len;
            continue;
        }
        let raw_name = &record[33..33 + name_len];
        let name = String::from_utf8_lossy(raw_name).to_string();
        let is_dir = flags & 0x02 != 0;

        if !is_dir && extent == target_lba && size >= target_size {
            return Some(format!("/{}", name.trim_end_matches(";1")));
        }

        if is_dir && name != "." && name != ".." && name_len > 1 {
            if let Some(found) = find_iso_path_by_lba(
                iso,
                _pvd,
                extent,
                size,
                target_lba,
                target_size,
                depth + 1,
                visited,
            ) {
                return Some(format!("/{}/{}", name, found.trim_start_matches('/')));
            }
        }

        offset += record_len;
    }

    None
}

fn parse_lsdl_line(line: &str) -> Option<IsoFile> {
    if !line.starts_with('-') {
        return None;
    }
    let size = line.split_whitespace().nth(4)?.parse::<u64>().ok()?;

    let mut rest = line;
    for _ in 0..8 {
        let index = rest.find(char::is_whitespace)?;
        rest = rest[index..].trim_start();
    }

    let path = rest.trim().trim_matches('\'');
    if path.is_empty() {
        return None;
    }

    Some(IsoFile {
        path: if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        },
        size,
    })
}

fn list_files_with_xorriso(iso: &Path) -> CoreResult<Vec<IsoFile>> {
    let xorriso = tool_or_error("xorriso")?;
    let output = run(
        &xorriso,
        &[
            "-indev".to_string(),
            iso.display().to_string(),
            "-find".to_string(),
            "/".to_string(),
            "-type".to_string(),
            "f".to_string(),
            "-exec".to_string(),
            "lsdl".to_string(),
        ],
    )?;

    if !output.success {
        return Err(CoreError::new(format!(
            "xorriso não conseguiu listar a ISO: {}",
            output.stderr.trim()
        )));
    }

    let mut files: Vec<IsoFile> = output.stdout.lines().filter_map(parse_lsdl_line).collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn list_files_with_7z(iso: &Path, udf: bool) -> CoreResult<Vec<IsoFile>> {
    let seven_zip = tool_or_error("7z")?;
    let mut args = vec!["l".to_string(), "-slt".to_string()];
    if udf {
        args.push("-tUdf".to_string());
    }
    args.push(iso.display().to_string());
    let output = run(&seven_zip, &args)?;
    if !output.success {
        return Err(CoreError::new(format!(
            "7z não conseguiu listar a ISO: {}",
            output.stderr.trim()
        )));
    }

    let mut files = Vec::new();
    let mut path: Option<String> = None;
    let mut size = 0u64;
    let mut is_dir = false;

    let flush =
        |files: &mut Vec<IsoFile>, path: &mut Option<String>, size: &mut u64, is_dir: &mut bool| {
            if let Some(value) = path.take() {
                if !*is_dir {
                    let normalized = if value.starts_with('/') {
                        value
                    } else {
                        format!("/{value}")
                    };
                    files.push(IsoFile {
                        path: normalized.replace('\\', "/"),
                        size: *size,
                    });
                }
            }
            *size = 0;
            *is_dir = false;
        };

    for line in output.stdout.lines() {
        if line.trim().is_empty() {
            flush(&mut files, &mut path, &mut size, &mut is_dir);
        } else if let Some(value) = line.strip_prefix("Path = ") {
            flush(&mut files, &mut path, &mut size, &mut is_dir);
            path = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Size = ") {
            size = value.trim().parse::<u64>().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("Folder = ") {
            is_dir = value.trim() == "+";
        }
    }
    flush(&mut files, &mut path, &mut size, &mut is_dir);

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

pub fn list_files_udf(iso: &Path) -> CoreResult<Vec<IsoFile>> {
    list_files_with_7z(iso, true)
}

pub fn list_files_iso9660(iso: &Path) -> CoreResult<Vec<IsoFile>> {
    list_files_with_xorriso(iso)
}

/// The UDF layer is authoritative when present: Microsoft Windows images can
/// expose a limited-size file in ISO9660 while the full file exists only in
/// UDF. Falling back to ISO9660 for a UDF image would silently lose content.
pub fn list_files(iso: &Path, metadata: &IsoMetadata) -> CoreResult<Vec<IsoFile>> {
    if metadata.has_udf {
        return list_files_udf(iso);
    }
    if metadata.has_iso9660 {
        return list_files_iso9660(iso);
    }
    Err(CoreError::new("Imagem sem ISO9660 ou UDF reconhecível."))
}

pub fn inspect(iso: &Path) -> CoreResult<IsoMetadata> {
    let size = std::fs::metadata(iso)
        .map_err(|e| CoreError::new(format!("ISO '{}' inacessível: {e}", iso.display())))?
        .len();

    let mut meta = IsoMetadata {
        path: iso.to_path_buf(),
        size,
        ..Default::default()
    };

    let pvd = match read_pvd(iso) {
        Ok(pvd) => {
            meta.has_iso9660 = true;
            meta.volume_id = if pvd.volume_id.is_empty() {
                None
            } else {
                Some(pvd.volume_id.clone())
            };
            meta.system_id = if pvd.system_id.is_empty() {
                None
            } else {
                Some(pvd.system_id.clone())
            };
            meta.iso9660_1999 = pvd.version >= 2;
            Some(pvd)
        }
        Err(err) => {
            meta.warnings.push(err.to_string());
            None
        }
    };

    meta.has_joliet = detect_joliet(iso);
    if let Some(pvd) = &pvd {
        meta.has_rock_ridge = detect_rock_ridge(iso, pvd);
    }

    if let Some(isoinfo) = locate_tool("isoinfo") {
        if let Ok(output) = run(
            &isoinfo,
            &[
                "-d".to_string(),
                "-i".to_string(),
                iso.display().to_string(),
            ],
        ) {
            if output.stdout.contains("ISO 9660:1999") {
                meta.iso9660_1999 = true;
            }
        }
    }

    if let Some(version) = detect_udf(iso) {
        meta.has_udf = true;
        meta.udf_version = Some(version);
    }

    if let Some(blkid) = locate_tool("blkid") {
        if let Ok(output) = run(
            &blkid,
            &[
                "-p".to_string(),
                "-o".to_string(),
                "export".to_string(),
                iso.display().to_string(),
            ],
        ) {
            for line in output.stdout.lines() {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key {
                    "TYPE" => {
                        if value.eq_ignore_ascii_case("udf") {
                            meta.has_udf = true;
                        } else if value.eq_ignore_ascii_case("iso9660") {
                            meta.has_iso9660 = true;
                        }
                    }
                    "VERSION" if meta.has_udf => {
                        meta.udf_version = Some(value.trim_matches('"').to_string());
                    }
                    "VOLUME_ID" | "LABEL" if meta.volume_id.is_none() => {
                        let value = value.trim_matches('"');
                        if !value.is_empty() {
                            meta.volume_id = Some(value.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let (has_mbr, has_gpt, mut system_area_information) = detect_system_area(iso);
    meta.has_mbr = has_mbr;
    meta.has_gpt = has_gpt;

    let el_torito = if let Some(xorriso) = locate_tool("xorriso") {
        match run(
            &xorriso,
            &[
                "-indev".to_string(),
                iso.display().to_string(),
                "-report_el_torito".to_string(),
                "plain".to_string(),
            ],
        ) {
            Ok(output) => {
                let combined = format!("{}\n{}", output.stdout, output.stderr);
                parse_el_torito_report(&combined).or_else(|| {
                    pvd.as_ref()
                        .and_then(|pvd| parse_direct_el_torito(iso, pvd))
                })
            }
            Err(err) => {
                meta.warnings
                    .push(format!("Falha ao consultar El Torito via xorriso: {err}"));
                pvd.as_ref()
                    .and_then(|pvd| parse_direct_el_torito(iso, pvd))
            }
        }
    } else {
        meta.warnings
            .push("xorriso não encontrado; inspeção El Torito limitada.".to_string());
        pvd.as_ref()
            .and_then(|pvd| parse_direct_el_torito(iso, pvd))
    };

    if let Some(data) = el_torito {
        meta.has_el_torito = data.has_el_torito;
        meta.boot_catalog = data.catalog_path;
        meta.boot_catalog_lba = data.catalog_lba;
        meta.boot_entries = data.entries;
        meta.has_bios_boot = meta
            .boot_entries
            .iter()
            .any(|entry| entry.is_bios() && entry.bootable);
        meta.has_uefi_boot = meta
            .boot_entries
            .iter()
            .any(|entry| entry.is_uefi() && entry.bootable);
        meta.bios_boot_image = meta
            .boot_entries
            .iter()
            .find(|entry| entry.is_bios())
            .and_then(|entry| entry.image_path.clone());
        meta.uefi_boot_image = meta
            .boot_entries
            .iter()
            .find(|entry| entry.is_uefi())
            .and_then(|entry| entry.image_path.clone());
    }

    if let Some(xorriso) = locate_tool("xorriso") {
        if let Ok(output) = run(
            &xorriso,
            &[
                "-indev".to_string(),
                iso.display().to_string(),
                "-report_system_area".to_string(),
                "plain".to_string(),
            ],
        ) {
            let combined = format!("{}\n{}", output.stdout, output.stderr);
            if combined.contains("No System Area was loaded") {
                system_area_information = "Sem área de sistema MBR/GPT".to_string();
            } else if combined.contains("System area") || combined.contains("MBR") {
                let report: Vec<&str> = combined
                    .lines()
                    .filter(|line| {
                        line.contains("System area")
                            || line.contains("MBR")
                            || line.contains("GPT")
                            || line.contains("APM")
                    })
                    .collect();
                if !report.is_empty() {
                    system_area_information = report.join("; ");
                }
            }
        }
    }
    meta.system_area_information = system_area_information;
    meta.has_system_area = meta.has_mbr || meta.has_gpt;

    if meta.has_iso9660 {
        meta.filesystem_types.push(if meta.iso9660_1999 {
            "ISO9660:1999".to_string()
        } else {
            "ISO9660".to_string()
        });
    }
    if meta.has_joliet {
        meta.filesystem_types.push("Joliet".to_string());
    }
    if meta.has_rock_ridge {
        meta.filesystem_types.push("Rock Ridge".to_string());
    }
    if meta.has_udf {
        meta.filesystem_types.push(format!(
            "UDF {}",
            meta.udf_version.as_deref().unwrap_or("desconhecida")
        ));
    }

    let mut udf_files = None;
    let mut iso_files = None;

    if meta.has_udf {
        match list_files_udf(iso) {
            Ok(files) => udf_files = Some(files),
            Err(err) => meta.warnings.push(format!(
                "Não foi possível listar a camada UDF com 7z: {err}.                  A edição/exportação UDF será bloqueada."
            )),
        }
    }
    if meta.has_iso9660 {
        match list_files_iso9660(iso) {
            Ok(files) => iso_files = Some(files),
            Err(err) => meta
                .warnings
                .push(format!("Não foi possível listar a camada ISO9660: {err}")),
        }
    }

    if let (Some(iso_files), Some(udf_files)) = (&iso_files, &udf_files) {
        let iso_map: std::collections::HashMap<String, u64> = iso_files
            .iter()
            .map(|file| (file.path.to_ascii_lowercase(), file.size))
            .collect();
        let mut differences = Vec::new();
        for udf_file in udf_files {
            let key = udf_file.path.to_ascii_lowercase();
            if let Some(iso_size) = iso_map.get(&key) {
                if *iso_size != udf_file.size {
                    differences.push(format!(
                        "{}: ISO9660 {} vs UDF {}",
                        udf_file.path, iso_size, udf_file.size
                    ));
                    if differences.len() >= 8 {
                        break;
                    }
                }
            }
        }
        if !differences.is_empty() {
            meta.warnings.push(format!(
                "As camadas ISO9660 e UDF divergem; a UDF é a fonte de verdade e será usada na extração: {}",
                differences.join("; ")
            ));
        }
    }

    let files = udf_files.or(iso_files).unwrap_or_default();
    if !files.is_empty() {
        meta.essential_files = essential_files_from(&files);
    } else {
        meta.warnings.push(
            "Não foi possível obter a lista de arquivos da ISO; a validação será limitada."
                .to_string(),
        );
    }

    Ok(meta)
}

pub fn read_boot_image(iso: &Path, entry: &BootEntry) -> CoreResult<Vec<u8>> {
    let len = entry.byte_len();
    let meta = std::fs::metadata(iso)?;
    let offset = entry.lba.saturating_mul(SECTOR);
    if offset.saturating_add(len) > meta.len() {
        return Err(CoreError::new(format!(
            "Imagem de boot {} em LBA {} excede o tamanho da ISO.",
            entry.index, entry.lba
        )));
    }
    read_at(iso, offset, len as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspects_iso_from_env() {
        let Ok(path) = std::env::var("ISOFORGE_TEST_ISO") else {
            return;
        };
        let metadata = inspect(Path::new(&path)).expect("inspeção deve funcionar");
        eprintln!("{metadata:#?}");
        assert!(metadata.has_iso9660 || metadata.has_udf);
        assert!(!metadata.filesystem_types.is_empty());
    }
}
