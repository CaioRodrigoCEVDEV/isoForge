use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct IsoMetadata {
    pub path: PathBuf,
    pub size: u64,
    pub volume_id: Option<String>,
    pub system_id: Option<String>,
    pub filesystem_types: Vec<String>,
    pub has_iso9660: bool,
    pub iso9660_1999: bool,
    pub has_joliet: bool,
    pub has_rock_ridge: bool,
    pub has_udf: bool,
    pub udf_version: Option<String>,
    pub has_el_torito: bool,
    pub has_bios_boot: bool,
    pub has_uefi_boot: bool,
    pub boot_catalog: Option<String>,
    pub boot_catalog_lba: Option<u64>,
    pub bios_boot_image: Option<String>,
    pub uefi_boot_image: Option<String>,
    pub boot_entries: Vec<BootEntry>,
    pub system_area_information: String,
    pub has_system_area: bool,
    pub has_mbr: bool,
    pub has_gpt: bool,
    pub essential_files: Vec<EssentialFile>,
    pub warnings: Vec<String>,
}

impl IsoMetadata {
    pub fn is_hybrid_iso9660_udf(&self) -> bool {
        self.has_iso9660 && self.has_udf
    }

    pub fn is_udf_only(&self) -> bool {
        self.has_udf && !self.has_iso9660
    }

    pub fn boot_entries_for(&self, platform: &str) -> Vec<&BootEntry> {
        self.boot_entries
            .iter()
            .filter(|entry| entry.platform.eq_ignore_ascii_case(platform))
            .collect()
    }

    pub fn has_windows_layout(&self) -> bool {
        self.essential_files.iter().any(|file| {
            file.present
                && matches!(
                    file.path.to_ascii_lowercase().as_str(),
                    "/bootmgr" | "/sources/install.wim" | "/sources/install.esd"
                )
        })
    }

    pub fn filesystem_summary(&self) -> String {
        if self.filesystem_types.is_empty() {
            "desconhecido".to_string()
        } else {
            self.filesystem_types.join(" + ")
        }
    }

    pub fn boot_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.has_bios_boot {
            parts.push("BIOS");
        }
        if self.has_uefi_boot {
            parts.push("UEFI");
        }
        if parts.is_empty() {
            "sem boot El Torito".to_string()
        } else {
            parts.join(" + ")
        }
    }
}

#[derive(Clone, Debug)]
pub struct BootEntry {
    /// 1-based El Torito entry number.
    pub index: u32,
    pub platform: String,
    pub bootable: bool,
    pub emulation: String,
    pub load_segment: String,
    pub header_points: String,
    /// Number of 512-byte sectors loaded by the firmware.
    pub load_size: u32,
    pub lba: u64,
    pub image_path: Option<String>,
}

impl BootEntry {
    pub fn is_bios(&self) -> bool {
        self.platform.eq_ignore_ascii_case("BIOS")
    }

    pub fn is_uefi(&self) -> bool {
        self.platform.eq_ignore_ascii_case("UEFI")
    }

    pub fn byte_len(&self) -> u64 {
        self.load_size.max(1) as u64 * 512
    }
}

#[derive(Clone, Debug)]
pub struct EssentialFile {
    pub path: String,
    pub size: Option<u64>,
    pub present: bool,
}

#[derive(Clone, Debug)]
pub struct IsoFile {
    pub path: String,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportLevel {
    Full,
    ReadOnly,
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct BackendCapability {
    pub backend: String,
    pub level: SupportLevel,
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub estimated_output_size: u64,
}

impl BackendCapability {
    pub fn is_supported(&self) -> bool {
        self.level == SupportLevel::Full
    }
}

pub const ESSENTIAL_PATHS: &[&str] = &[
    "/bootmgr",
    "/boot/bcd",
    "/boot/bootsect.exe",
    "/boot/etfsboot.com",
    "/efi/boot/bootx64.efi",
    "/efi/microsoft/boot/bootmgfw.efi",
    "/efi/microsoft/boot/efisys.bin",
    "/sources/install.wim",
    "/sources/install.esd",
    "/sources/boot.wim",
    "/setup.exe",
    "/autounattend.xml",
];

pub fn find_file<'a>(files: &'a [IsoFile], path: &str) -> Option<&'a IsoFile> {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    files
        .iter()
        .find(|file| file.path.to_ascii_lowercase() == normalized)
}

pub fn essential_files_from(files: &[IsoFile]) -> Vec<EssentialFile> {
    ESSENTIAL_PATHS
        .iter()
        .map(|path| {
            let found = find_file(files, path);
            EssentialFile {
                path: (*path).to_string(),
                size: found.map(|file| file.size),
                present: found.is_some(),
            }
        })
        .collect()
}
