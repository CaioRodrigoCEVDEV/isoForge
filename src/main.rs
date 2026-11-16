use gtk::prelude::*;
use gtk::{
    gio, glib, Application, ApplicationWindow, Box as GtkBox, Button, FileChooserAction,
    FileChooserNative, Grid, Image, Label, ListBox, ListBoxRow, Orientation, ProgressBar,
    ResponseType, ScrolledWindow, SelectionMode, Window,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

mod core;

use crate::core::fsutil::scan_tree;
use crate::core::iso_builder::{
    extract_iso, select_backend, BuildRequest, BuildResult, Progress as BuildProgress,
};
use crate::core::iso_inspector::inspect;
use crate::core::iso_validator::{validate, CheckStatus, FinalStatus, ValidationReport};
use crate::core::model::{BackendCapability, IsoMetadata, SupportLevel};
use crate::core::process::{available_space, create_workspace, CancelToken};

struct Workspace {
    _temp: TempDir,
    root: PathBuf,
    source_iso: PathBuf,
    metadata: IsoMetadata,
}

struct Ui {
    window: ApplicationWindow,
    list: ListBox,
    scroll: ScrolledWindow,
    info: Label,
    status: Label,
    progress_row: GtkBox,
    progress: ProgressBar,
    cancel_btn: Button,
    open_btn: Button,
    add_btn: Button,
    remove_btn: Button,
    export_btn: Button,
}

#[derive(Clone, Debug)]
struct FileEntry {
    path: PathBuf,
    name: String,
    relative: String,
    is_dir: bool,
    depth: usize,
    size: u64,
}

enum OpenMsg {
    Phase(String),
    Progress { done: u64, total: u64 },
    Done(Result<Workspace, String>),
}

enum ExportMsg {
    Progress(BuildProgress),
    Done(Result<(BuildResult, ValidationReport), String>),
}

fn main() {
    let app = Application::builder()
        .application_id("io.github.isoforge.ISOForge")
        .build();

    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("ISOForge")
        .default_width(1020)
        .default_height(720)
        .build();

    let outer = GtkBox::new(Orientation::Vertical, 8);
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);

    let header = GtkBox::new(Orientation::Horizontal, 8);
    let title = Label::new(Some("ISOForge"));
    title.add_css_class("title-1");
    title.set_hexpand(true);
    title.set_halign(gtk::Align::Start);

    let open_btn = Button::with_label("Abrir ISO");
    let add_btn = Button::with_label("Adicionar arquivo");
    let remove_btn = Button::with_label("Remover selecionado");
    let export_btn = Button::with_label("Inspecionar e exportar");

    header.append(&title);
    header.append(&open_btn);
    header.append(&add_btn);
    header.append(&remove_btn);
    header.append(&export_btn);

    let info = Label::new(Some(
        "Abra uma ISO para inspecionar o formato e criar um espaço de trabalho temporário.",
    ));
    info.set_halign(gtk::Align::Start);
    info.set_wrap(true);
    info.set_selectable(true);

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::Single);
    list.set_vexpand(true);
    list.set_activate_on_single_click(false);

    let scroll = ScrolledWindow::builder().vexpand(true).child(&list).build();

    let progress_row = GtkBox::new(Orientation::Horizontal, 8);
    progress_row.set_visible(false);

    let progress = ProgressBar::new();
    progress.set_show_text(true);
    progress.set_hexpand(true);

    let cancel_btn = Button::with_label("Cancelar");
    cancel_btn.set_visible(false);

    progress_row.append(&progress);
    progress_row.append(&cancel_btn);

    let warning = Label::new(Some(
        "A ISO original nunca é modificada. A exportação usa um workspace separado e valida a estrutura \
         gerada; sem teste de boot em VM, a ISO não é anunciada como inicializável.",
    ));
    warning.add_css_class("warning");
    warning.set_halign(gtk::Align::Start);
    warning.set_wrap(true);

    let status = Label::new(Some("Pronto."));
    status.set_halign(gtk::Align::Start);
    status.set_wrap(true);
    status.set_selectable(true);

    outer.append(&header);
    outer.append(&info);
    outer.append(&scroll);
    outer.append(&progress_row);
    outer.append(&warning);
    outer.append(&status);
    window.set_child(Some(&outer));

    let ui = Rc::new(Ui {
        window: window.clone(),
        list: list.clone(),
        scroll: scroll.clone(),
        info: info.clone(),
        status: status.clone(),
        progress_row: progress_row.clone(),
        progress: progress.clone(),
        cancel_btn: cancel_btn.clone(),
        open_btn: open_btn.clone(),
        add_btn: add_btn.clone(),
        remove_btn: remove_btn.clone(),
        export_btn: export_btn.clone(),
    });

    let workspace: Rc<RefCell<Option<Workspace>>> = Rc::new(RefCell::new(None));
    let expanded: Rc<RefCell<HashSet<PathBuf>>> = Rc::new(RefCell::new(HashSet::new()));
    let rows: Rc<RefCell<Vec<FileEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let active_cancel: Rc<RefCell<Option<CancelToken>>> = Rc::new(RefCell::new(None));

    // Cancel the running extraction/rebuild.
    {
        let ui = ui.clone();
        let active_cancel = active_cancel.clone();
        let cancel_btn = ui.cancel_btn.clone();
        cancel_btn.connect_clicked(move |_| {
            if let Some(token) = active_cancel.borrow().as_ref() {
                token.cancel();
            }
            ui.status.set_text("Cancelando operação...");
        });
    }

    // Open ISO
    {
        let ui = ui.clone();
        let workspace = workspace.clone();
        let expanded = expanded.clone();
        let rows = rows.clone();
        let active_cancel = active_cancel.clone();

        let open_btn = ui.open_btn.clone();
        open_btn.connect_clicked(move |_| {
            let dialog = FileChooserNative::new(
                Some("Abrir imagem ISO"),
                Some(&ui.window),
                FileChooserAction::Open,
                Some("Abrir"),
                Some("Cancelar"),
            );

            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Imagens ISO"));
            filter.add_pattern("*.iso");
            filter.add_pattern("*.ISO");
            dialog.add_filter(&filter);

            let ui = ui.clone();
            let workspace = workspace.clone();
            let expanded = expanded.clone();
            let rows = rows.clone();
            let active_cancel = active_cancel.clone();

            dialog.connect_response(move |dialog, response| {
                if response != ResponseType::Accept {
                    return;
                }

                let Some(iso_path) = dialog.file().and_then(|f| f.path()) else {
                    ui.status
                        .set_text("Não foi possível obter o caminho da ISO.");
                    return;
                };

                start_open(&ui, &workspace, &expanded, &rows, &active_cancel, iso_path);
            });
            dialog.show();
        });
    }

    // Add file
    {
        let ui = ui.clone();
        let workspace = workspace.clone();
        let expanded = expanded.clone();
        let rows = rows.clone();

        let add_btn = ui.add_btn.clone();
        add_btn.connect_clicked(move |_| {
            if workspace.borrow().is_none() {
                ui.status.set_text("Abra uma ISO primeiro.");
                return;
            }

            let dialog = FileChooserNative::new(
                Some("Adicionar arquivo ao workspace"),
                Some(&ui.window),
                FileChooserAction::Open,
                Some("Adicionar"),
                Some("Cancelar"),
            );

            let ui = ui.clone();
            let workspace = workspace.clone();
            let expanded = expanded.clone();
            let rows = rows.clone();

            dialog.connect_response(move |dialog, response| {
                if response != ResponseType::Accept {
                    return;
                }

                let Some(source) = dialog.file().and_then(|f| f.path()) else {
                    ui.status.set_text("Arquivo de origem inválido.");
                    return;
                };

                let Some(root) = workspace.borrow().as_ref().map(|w| w.root.clone()) else {
                    ui.status.set_text("Nenhum workspace aberto.");
                    return;
                };

                let Some(name) = source.file_name() else {
                    ui.status.set_text("Nome de arquivo inválido.");
                    return;
                };

                let destination = root.join(name);
                if destination.exists() {
                    ui.status
                        .set_text("Já existe um arquivo com esse nome na raiz.");
                    return;
                }

                let source_size = fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
                let free = available_space(&root).unwrap_or(u64::MAX);
                if free < source_size.saturating_add(64 * 1024 * 1024) {
                    ui.status.set_text(&format!(
                        "Espaço insuficiente no workspace para copiar {}.",
                        human_size(source_size)
                    ));
                    return;
                }

                match fs::copy(&source, &destination) {
                    Ok(_) => match refresh_file_list(
                        &ui.list, &ui.scroll, &ui.status, &rows, &expanded, &root,
                    ) {
                        Ok(_) => ui.status.set_text("Arquivo adicionado ao workspace."),
                        Err(e) => ui.status.set_text(&format!("Erro ao listar arquivos: {e}")),
                    },
                    Err(e) => ui.status.set_text(&format!("Falha ao copiar arquivo: {e}")),
                }
            });
            dialog.show();
        });
    }

    // Remove selected item
    {
        let ui = ui.clone();
        let workspace = workspace.clone();
        let expanded = expanded.clone();
        let rows = rows.clone();

        let remove_btn = ui.remove_btn.clone();
        remove_btn.connect_clicked(move |_| {
            let Some(root) = workspace.borrow().as_ref().map(|w| w.root.clone()) else {
                ui.status.set_text("Nenhum workspace aberto.");
                return;
            };

            let Some(row) = ui.list.selected_row() else {
                ui.status.set_text("Selecione um arquivo primeiro.");
                return;
            };

            let index = row.index();
            if index < 0 {
                return;
            }

            let Some(entry) = rows.borrow().get(index as usize).cloned() else {
                return;
            };

            if !entry.path.starts_with(&root) || entry.path == root {
                ui.status.set_text("Caminho inválido.");
                return;
            }

            let result = if entry.is_dir {
                fs::remove_dir_all(&entry.path)
            } else {
                fs::remove_file(&entry.path)
            };

            match result {
                Ok(_) => {
                    expanded
                        .borrow_mut()
                        .retain(|path| !path.starts_with(&entry.path));
                    match refresh_file_list(
                        &ui.list, &ui.scroll, &ui.status, &rows, &expanded, &root,
                    ) {
                        Ok(_) => ui.status.set_text("Item removido do workspace."),
                        Err(e) => ui.status.set_text(&format!("Erro ao listar arquivos: {e}")),
                    }
                }
                Err(e) => ui.status.set_text(&format!("Falha ao remover: {e}")),
            }
        });
    }

    // Export: open the inspection dialog before touching the destination.
    {
        let ui = ui.clone();
        let workspace = workspace.clone();
        let active_cancel = active_cancel.clone();

        let export_btn = ui.export_btn.clone();
        export_btn.connect_clicked(move |_| {
            if workspace.borrow().is_none() {
                ui.status.set_text("Abra uma ISO primeiro.");
                return;
            }

            let dialog = FileChooserNative::new(
                Some("Exportar nova ISO"),
                Some(&ui.window),
                FileChooserAction::Save,
                Some("Continuar"),
                Some("Cancelar"),
            );
            dialog.set_current_name("isoforge-custom.iso");

            let ui = ui.clone();
            let workspace = workspace.clone();
            let active_cancel = active_cancel.clone();

            dialog.connect_response(move |dialog, response| {
                if response != ResponseType::Accept {
                    return;
                }

                let Some(output) = dialog.file().and_then(|f| f.path()) else {
                    ui.status.set_text("Caminho de saída inválido.");
                    return;
                };

                show_inspection_dialog(&ui, &workspace, &active_cancel, output);
            });
            dialog.show();
        });
    }

    window.present();
}

fn set_busy(ui: &Ui, busy: bool) {
    for button in [&ui.open_btn, &ui.add_btn, &ui.remove_btn, &ui.export_btn] {
        button.set_sensitive(!busy);
    }
}

fn begin_task(ui: &Ui, active_cancel: &Rc<RefCell<Option<CancelToken>>>, cancel: CancelToken) {
    set_busy(ui, true);
    ui.progress_row.set_visible(true);
    ui.progress.set_fraction(0.0);
    ui.cancel_btn.set_visible(true);
    *active_cancel.borrow_mut() = Some(cancel);
}

fn end_task(ui: &Ui, active_cancel: &Rc<RefCell<Option<CancelToken>>>) {
    *active_cancel.borrow_mut() = None;
    ui.cancel_btn.set_visible(false);
    ui.progress_row.set_visible(false);
    set_busy(ui, false);
}

fn start_open(
    ui: &Rc<Ui>,
    workspace: &Rc<RefCell<Option<Workspace>>>,
    expanded: &Rc<RefCell<HashSet<PathBuf>>>,
    rows: &Rc<RefCell<Vec<FileEntry>>>,
    active_cancel: &Rc<RefCell<Option<CancelToken>>>,
    iso: PathBuf,
) {
    let cancel = CancelToken::new();
    begin_task(ui, active_cancel, cancel.clone());
    ui.status.set_text("Inspecionando a ISO...");
    ui.progress.set_text(Some("Inspecionando a ISO..."));
    clear_list(&ui.list);
    rows.borrow_mut().clear();

    let (tx, rx) = mpsc::channel::<OpenMsg>();
    std::thread::spawn(move || {
        let result = open_workspace(&iso, &cancel, &tx);
        let _ = tx.send(OpenMsg::Done(result));
    });

    let ui = ui.clone();
    let workspace = workspace.clone();
    let expanded = expanded.clone();
    let rows = rows.clone();
    let active_cancel = Rc::clone(active_cancel);

    glib::timeout_add_local(Duration::from_millis(100), move || loop {
        match rx.try_recv() {
            Ok(OpenMsg::Phase(text)) => {
                ui.status.set_text(&text);
                ui.progress.set_text(Some(&text));
                ui.progress.pulse();
            }
            Ok(OpenMsg::Progress { done, total }) => {
                if total > 0 {
                    ui.progress
                        .set_fraction((done as f64 / total as f64).clamp(0.0, 0.99));
                    ui.progress.set_text(Some(&format!(
                        "Extraindo... {} de {}",
                        human_size(done),
                        human_size(total)
                    )));
                } else {
                    ui.progress.pulse();
                    ui.progress
                        .set_text(Some(&format!("Extraindo... {}", human_size(done))));
                }
            }
            Ok(OpenMsg::Done(result)) => {
                end_task(&ui, &active_cancel);

                match result {
                    Ok(ws) => {
                        let root = ws.root.clone();
                        let source = ws.source_iso.clone();
                        let metadata = ws.metadata.clone();
                        *workspace.borrow_mut() = Some(ws);
                        expanded.borrow_mut().clear();

                        ui.info.set_text(&format_info(&source, &root, &metadata));

                        let adjustment = ui.scroll.vadjustment();
                        adjustment.set_value(adjustment.lower());

                        match refresh_file_list(
                            &ui.list, &ui.scroll, &ui.status, &rows, &expanded, &root,
                        ) {
                            Ok(visible) => {
                                ui.list.unselect_all();
                                let total = scan_tree(&root).0;
                                let mut text = format!(
                                    "ISO aberta. {total} arquivos extraídos ({visible} itens visíveis). \
                                     Boot: {}. {}",
                                    metadata.boot_summary(),
                                    if metadata.has_udf {
                                        format!(
                                            "UDF {} preservado no workspace.",
                                            metadata.udf_version.as_deref().unwrap_or("detectado")
                                        )
                                    } else {
                                        "Sem UDF.".to_string()
                                    }
                                );
                                if !metadata.warnings.is_empty() {
                                    text.push_str(&format!(
                                        " Avisos: {}",
                                        metadata.warnings.join(" | ")
                                    ));
                                }
                                ui.status.set_text(&text);
                            }
                            Err(e) => ui.status.set_text(&format!("Erro ao listar arquivos: {e}")),
                        }
                    }
                    Err(e) => {
                        ui.status.set_text(&format!("Falha ao abrir ISO: {e}"));
                        if let Some(root) = workspace.borrow().as_ref().map(|w| w.root.clone()) {
                            let _ = refresh_file_list(
                                &ui.list, &ui.scroll, &ui.status, &rows, &expanded, &root,
                            );
                        }
                    }
                }

                return glib::ControlFlow::Break;
            }
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                end_task(&ui, &active_cancel);
                ui.status
                    .set_text("A operação foi interrompida inesperadamente.");
                return glib::ControlFlow::Break;
            }
        }
    });
}

fn open_workspace(
    iso: &Path,
    cancel: &CancelToken,
    tx: &mpsc::Sender<OpenMsg>,
) -> Result<Workspace, String> {
    let metadata = inspect(iso).map_err(|e| e.to_string())?;

    let _ = tx.send(OpenMsg::Phase(format!(
        "Inspecionando: volume '{}', {}.",
        metadata.volume_id.as_deref().unwrap_or("sem ID"),
        metadata.filesystem_summary()
    )));

    let temp = create_workspace().map_err(|e| e.to_string())?;
    let root = temp.path().join("contents");
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    let free = available_space(temp.path()).map_err(|e| e.to_string())?;
    let required = metadata.size.saturating_add(256 * 1024 * 1024);
    if free < required {
        return Err(format!(
            "Espaço insuficiente em '{}' para o workspace: necessário ~{} GB, disponível ~{} GB. \
             Defina ISOFORGE_WORK_DIR para um disco com mais espaço.",
            temp.path().display(),
            required / 1_000_000_000,
            free / 1_000_000_000
        ));
    }

    extract_iso(iso, &metadata, &root, cancel, &mut |progress| {
        let _ = match progress {
            BuildProgress::Stage(text) => tx.send(OpenMsg::Phase(text)),
            BuildProgress::Bytes { done, total } => tx.send(OpenMsg::Progress { done, total }),
        };
    })
    .map_err(|e| e.to_string())?;

    Ok(Workspace {
        _temp: temp,
        root,
        source_iso: iso.to_path_buf(),
        metadata,
    })
}

fn show_inspection_dialog(
    ui: &Rc<Ui>,
    workspace: &Rc<RefCell<Option<Workspace>>>,
    active_cancel: &Rc<RefCell<Option<CancelToken>>>,
    output: PathBuf,
) {
    let Some((root, metadata)) = workspace
        .borrow()
        .as_ref()
        .map(|workspace| (workspace.root.clone(), workspace.metadata.clone()))
    else {
        ui.status.set_text("Nenhum workspace aberto.");
        return;
    };

    let capability = select_backend(&metadata).capability(&metadata, Some(&root));

    let dialog = Window::builder()
        .title("ISOForge — Inspeção da ISO")
        .transient_for(&ui.window)
        .modal(true)
        .default_width(680)
        .build();

    let outer = GtkBox::new(Orientation::Vertical, 12);
    outer.set_margin_top(16);
    outer.set_margin_bottom(16);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    let title = Label::new(Some("Inspeção antes da reconstrução"));
    title.add_css_class("title-2");
    title.set_halign(gtk::Align::Start);

    let subtitle = Label::new(Some(&format!("Destino: {}", output.display())));
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.set_selectable(true);
    subtitle.set_opacity(0.8);

    let grid = Grid::builder()
        .row_spacing(6)
        .column_spacing(16)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    let rows = metadata_rows(&metadata, &capability);
    for (index, (label, value)) in rows.iter().enumerate() {
        let key = Label::new(Some(label));
        key.set_halign(gtk::Align::Start);
        key.add_css_class("dim-label");
        let value = Label::new(Some(value));
        value.set_halign(gtk::Align::Start);
        value.set_wrap(true);
        value.set_selectable(true);
        value.set_hexpand(true);
        grid.attach(&key, 0, index as i32, 1, 1);
        grid.attach(&value, 1, index as i32, 1, 1);
    }

    let content = GtkBox::new(Orientation::Vertical, 10);
    content.append(&title);
    content.append(&subtitle);
    content.append(&grid);

    if !capability.blockers.is_empty() {
        let label = Label::new(Some(&format!(
            "Reconstrução bloqueada:\n• {}",
            capability.blockers.join("\n• ")
        )));
        label.add_css_class("error");
        label.set_halign(gtk::Align::Start);
        label.set_wrap(true);
        label.set_selectable(true);
        content.append(&label);
    }

    let mut warnings = capability.warnings.clone();
    warnings.extend(metadata.warnings.clone());
    if !warnings.is_empty() {
        let label = Label::new(Some(&format!("Avisos:\n• {}", warnings.join("\n• "))));
        label.add_css_class("warning");
        label.set_halign(gtk::Align::Start);
        label.set_wrap(true);
        label.set_selectable(true);
        content.append(&label);
    }

    let content_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .child(&content)
        .build();
    outer.append(&content_scroll);

    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = Button::with_label("Cancelar");
    let export = Button::with_label("Exportar nova ISO");
    export.add_css_class("suggested-action");
    if !capability.is_supported() {
        export.set_sensitive(false);
        export.set_tooltip_text(Some(
            "O backend não pode reconstruir esta imagem com segurança.",
        ));
    }
    buttons.append(&cancel);
    buttons.append(&export);
    outer.append(&buttons);

    dialog.set_child(Some(&outer));

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let ui = ui.clone();
        let workspace = workspace.clone();
        let active_cancel = Rc::clone(active_cancel);
        let capability = capability.clone();
        let output_button = output.clone();
        export.connect_clicked(move |_| {
            dialog.close();
            start_export(
                &ui,
                &workspace,
                &active_cancel,
                output_button.clone(),
                capability.clone(),
            );
        });
    }

    dialog.present();
}

fn metadata_rows(metadata: &IsoMetadata, capability: &BackendCapability) -> Vec<(String, String)> {
    let mut rows = vec![
        (
            "Arquivo original".to_string(),
            metadata.path.display().to_string(),
        ),
        (
            "Volume ID".to_string(),
            metadata
                .volume_id
                .clone()
                .unwrap_or_else(|| "(não informado)".to_string()),
        ),
        (
            "Sistemas de arquivos".to_string(),
            metadata.filesystem_summary(),
        ),
        ("Tamanho original".to_string(), human_size(metadata.size)),
        (
            "Tamanho estimado da nova ISO".to_string(),
            human_size(capability.estimated_output_size),
        ),
        (
            "El Torito".to_string(),
            if metadata.has_el_torito {
                format!(
                    "detectado — catálogo {} (LBA {:?})",
                    metadata.boot_catalog.as_deref().unwrap_or("desconhecido"),
                    metadata.boot_catalog_lba
                )
            } else {
                "não detectado".to_string()
            },
        ),
        (
            "Boot BIOS".to_string(),
            metadata
                .bios_boot_image
                .clone()
                .map(|path| format!("detectado — {path}"))
                .unwrap_or_else(|| "não detectado".to_string()),
        ),
        (
            "Boot UEFI".to_string(),
            metadata
                .uefi_boot_image
                .clone()
                .map(|path| format!("detectado — {path}"))
                .unwrap_or_else(|| "não detectado".to_string()),
        ),
        (
            "UDF".to_string(),
            if metadata.has_udf {
                format!(
                    "detectado — versão {}",
                    metadata.udf_version.as_deref().unwrap_or("desconhecida")
                )
            } else {
                "não detectado".to_string()
            },
        ),
        (
            "Joliet / Rock Ridge".to_string(),
            format!(
                "{} / {}",
                yes_no(metadata.has_joliet),
                yes_no(metadata.has_rock_ridge)
            ),
        ),
        (
            "Área de sistema".to_string(),
            metadata.system_area_information.clone(),
        ),
        (
            "Backend de reconstrução".to_string(),
            format!(
                "{} — {}",
                capability.backend,
                match capability.level {
                    SupportLevel::Full => "suportado",
                    SupportLevel::ReadOnly => "somente leitura",
                    SupportLevel::Unsupported => "não suportado",
                }
            ),
        ),
    ];

    if metadata.has_windows_layout() {
        rows.push((
            "Layout".to_string(),
            "Windows/WinPE detectado (BOOTMGR / install.wim)".to_string(),
        ));
    }

    let essentials: Vec<String> = metadata
        .essential_files
        .iter()
        .filter(|file| file.present)
        .map(|file| format!("{} ({})", file.path, human_size(file.size.unwrap_or(0))))
        .collect();
    if !essentials.is_empty() {
        rows.push(("Arquivos essenciais".to_string(), essentials.join(", ")));
    }

    for entry in &metadata.boot_entries {
        rows.push((
            format!("El Torito #{}", entry.index),
            format!(
                "{} — {} — emulação {} — carga {} setores — LSEG {} — HDPT {} — LBA {} — {}",
                entry.platform,
                if entry.bootable {
                    "bootável"
                } else {
                    "não bootável"
                },
                entry.emulation,
                entry.load_size,
                entry.load_segment,
                entry.header_points,
                entry.lba,
                entry.image_path.as_deref().unwrap_or("sem caminho")
            ),
        ));
    }

    rows
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "sim"
    } else {
        "não"
    }
}

fn format_info(source: &Path, root: &Path, metadata: &IsoMetadata) -> String {
    format!(
        "Origem: {}\nWorkspace: {}\nVolume ID: {} | {}\nBoot: {} | El Torito: {} | Avisos: {}",
        source.display(),
        root.display(),
        metadata.volume_id.as_deref().unwrap_or("(sem ID)"),
        metadata.filesystem_summary(),
        metadata.boot_summary(),
        yes_no(metadata.has_el_torito),
        if metadata.warnings.is_empty() {
            "nenhum".to_string()
        } else {
            metadata.warnings.join(" | ")
        }
    )
}

fn start_export(
    ui: &Rc<Ui>,
    workspace: &Rc<RefCell<Option<Workspace>>>,
    active_cancel: &Rc<RefCell<Option<CancelToken>>>,
    output: PathBuf,
    capability: BackendCapability,
) {
    let Some((root, metadata, source)) = workspace.borrow().as_ref().map(|workspace| {
        (
            workspace.root.clone(),
            workspace.metadata.clone(),
            workspace.source_iso.clone(),
        )
    }) else {
        ui.status.set_text("Nenhum workspace aberto.");
        return;
    };

    if !capability.is_supported() {
        ui.status.set_text(&format!(
            "Reconstrução não suportada: {}",
            capability.blockers.join(" | ")
        ));
        return;
    }

    if let (Ok(source_canon), Ok(output_canon)) =
        (fs::canonicalize(&source), fs::canonicalize(&output))
    {
        if source_canon == output_canon {
            ui.status
                .set_text("O destino não pode ser a ISO original. Escolha outro caminho.");
            return;
        }
    }

    let cancel = CancelToken::new();
    let cancel_for_worker = cancel.clone();
    begin_task(ui, active_cancel, cancel);

    ui.status
        .set_text("Reconstruindo ISO e preparando validação...");
    ui.progress.set_text(Some("Reconstruindo ISO..."));
    ui.progress.set_fraction(0.0);

    let (tx, rx) = mpsc::channel::<ExportMsg>();

    std::thread::spawn(move || {
        let backend = select_backend(&metadata);
        let request = BuildRequest {
            original: &source,
            tree: &root,
            metadata: &metadata,
            output: &output,
        };

        let mut progress = |progress: BuildProgress| {
            let _ = tx.send(ExportMsg::Progress(progress));
        };

        let result = backend.rebuild(&request, &cancel_for_worker, &mut progress);
        match result {
            Ok(build) => {
                let report = match inspect(&build.output) {
                    Ok(rebuilt_metadata) => {
                        validate(&source, &build.output, &metadata, &rebuilt_metadata, &root)
                    }
                    Err(err) => Ok(ValidationReport::invalid(
                        &build.output,
                        format!("Não foi possível inspecionar a ISO gerada: {err}"),
                    )),
                };
                let report = report.unwrap_or_else(|err| {
                    ValidationReport::invalid(&build.output, err.to_string())
                });
                let _ = tx.send(ExportMsg::Done(Ok((build, report))));
            }
            Err(err) => {
                let _ = tx.send(ExportMsg::Done(Err(err.to_string())));
            }
        }
    });

    let ui = ui.clone();
    let active_cancel = Rc::clone(active_cancel);
    glib::timeout_add_local(Duration::from_millis(100), move || loop {
        match rx.try_recv() {
            Ok(ExportMsg::Progress(progress)) => match progress {
                BuildProgress::Stage(text) => {
                    ui.status.set_text(&text);
                    ui.progress.set_text(Some(&text));
                    ui.progress.pulse();
                }
                BuildProgress::Bytes { done, total } => {
                    if total > 0 {
                        ui.progress
                            .set_fraction((done as f64 / total as f64).clamp(0.0, 0.99));
                        ui.progress.set_text(Some(&format!(
                            "Gravando ISO... {} de {}",
                            human_size(done),
                            human_size(total)
                        )));
                    } else {
                        ui.progress.pulse();
                        ui.progress
                            .set_text(Some(&format!("Gravando ISO... {}", human_size(done))));
                    }
                }
            },
            Ok(ExportMsg::Done(result)) => {
                end_task(&ui, &active_cancel);

                match result {
                    Ok((build, report)) => {
                        let status_text = match report.final_status {
                            FinalStatus::BootValidated => {
                                format!("ISO inicializável validada: {}", build.output.display())
                            }
                            FinalStatus::StructurallyValid => format!(
                                "ISO criada e validada estruturalmente: {}. \
                                 Boot NÃO testado em VM; não afirmamos que a imagem inicializa.",
                                build.output.display()
                            ),
                            FinalStatus::Invalid => format!(
                                "Reconstrução inválida. NÃO use '{}' para boot.",
                                build.output.display()
                            ),
                        };
                        ui.status.set_text(&status_text);
                        show_validation_dialog(&ui, &build, &report);
                    }
                    Err(err) => {
                        ui.status.set_text(&format!("Falha na reconstrução: {err}"));
                    }
                }

                return glib::ControlFlow::Break;
            }
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                end_task(&ui, &active_cancel);
                ui.status
                    .set_text("A reconstrução foi interrompida inesperadamente.");
                return glib::ControlFlow::Break;
            }
        }
    });
}

fn show_validation_dialog(ui: &Rc<Ui>, build: &BuildResult, report: &ValidationReport) {
    let dialog = Window::builder()
        .title("ISOForge — Relatório de reconstrução")
        .transient_for(&ui.window)
        .modal(true)
        .default_width(780)
        .default_height(580)
        .build();

    let outer = GtkBox::new(Orientation::Vertical, 10);
    outer.set_margin_top(16);
    outer.set_margin_bottom(16);
    outer.set_margin_start(16);
    outer.set_margin_end(16);

    let title = Label::new(Some("Relatório de reconstrução"));
    title.add_css_class("title-2");
    title.set_halign(gtk::Align::Start);
    outer.append(&title);

    let output = Label::new(Some(&format!(
        "Arquivo: {} | Backend: {}",
        report.output.display(),
        build.backend
    )));
    output.set_halign(gtk::Align::Start);
    output.set_wrap(true);
    output.set_selectable(true);
    output.set_opacity(0.8);
    outer.append(&output);

    let final_label = Label::new(Some(report.final_status.label()));
    final_label.set_halign(gtk::Align::Start);
    final_label.set_wrap(true);
    final_label.add_css_class(match report.final_status {
        FinalStatus::BootValidated => "success",
        FinalStatus::StructurallyValid => "warning",
        FinalStatus::Invalid => "error",
    });
    final_label.add_css_class("title-3");
    outer.append(&final_label);

    if report.has_failures() {
        let warning = Label::new(Some(
            "Uma ou mais verificações obrigatórias falharam. Esta imagem não deve ser \
             apresentada ou usada como inicializável.",
        ));
        warning.set_halign(gtk::Align::Start);
        warning.set_wrap(true);
        warning.add_css_class("error");
        outer.append(&warning);
    } else if report.has_warnings() {
        let note = Label::new(Some(
            "A validação estrutural passou, mas há avisos. O teste de boot em VM não foi \
             executado; não afirmamos que a imagem inicializa.",
        ));
        note.set_halign(gtk::Align::Start);
        note.set_wrap(true);
        note.add_css_class("warning");
        outer.append(&note);
    }

    let counts = Label::new(Some(&format!(
        "PASS: {} | WARNING/SKIP: {} | FAIL: {}",
        report.counts().0,
        report.counts().1,
        report.counts().2
    )));
    counts.set_halign(gtk::Align::Start);
    counts.set_opacity(0.8);
    outer.append(&counts);

    let scroll = ScrolledWindow::builder().vexpand(true).build();
    let content = GtkBox::new(Orientation::Vertical, 8);

    for check in &report.checks {
        let row = GtkBox::new(Orientation::Horizontal, 10);
        let status = Label::new(Some(check.status.label()));
        status.set_width_chars(8);
        status.set_halign(gtk::Align::Start);
        status.add_css_class(match check.status {
            CheckStatus::Pass => "success",
            CheckStatus::Warning => "warning",
            CheckStatus::Fail => "error",
        });
        let text = GtkBox::new(Orientation::Vertical, 2);
        text.set_hexpand(true);
        let label = Label::new(Some(&check.label));
        label.set_halign(gtk::Align::Start);
        label.set_wrap(true);
        let detail = Label::new(Some(&check.detail));
        detail.set_halign(gtk::Align::Start);
        detail.set_wrap(true);
        detail.set_selectable(true);
        detail.set_opacity(0.75);
        text.append(&label);
        text.append(&detail);
        row.append(&status);
        row.append(&text);
        content.append(&row);
    }

    if !build.actions.is_empty() {
        let title = Label::new(Some("Ações de reconstrução"));
        title.add_css_class("title-3");
        title.set_halign(gtk::Align::Start);
        content.append(&title);
        for action in &build.actions {
            let label = Label::new(Some(&format!("• {action}")));
            label.set_halign(gtk::Align::Start);
            label.set_wrap(true);
            label.set_selectable(true);
            content.append(&label);
        }
    }

    if !build.warnings.is_empty() {
        let title = Label::new(Some("Avisos da reconstrução"));
        title.add_css_class("title-3");
        title.set_halign(gtk::Align::Start);
        content.append(&title);
        for warning in &build.warnings {
            let label = Label::new(Some(&format!("• {warning}")));
            label.set_halign(gtk::Align::Start);
            label.set_wrap(true);
            label.set_selectable(true);
            label.add_css_class("warning");
            content.append(&label);
        }
    }

    scroll.set_child(Some(&content));
    outer.append(&scroll);

    let footer = GtkBox::new(Orientation::Horizontal, 8);
    footer.set_halign(gtk::Align::End);
    let close = Button::with_label("Fechar");
    close.add_css_class("suggested-action");
    footer.append(&close);
    outer.append(&footer);

    dialog.set_child(Some(&outer));
    {
        let dialog = dialog.clone();
        close.connect_clicked(move |_| dialog.close());
    }
    dialog.present();
}

// The active cancel token lives with the UI widgets. It is created in
// build_ui and passed explicitly to every background task.
fn collect_entries(
    dir: &Path,
    base: &Path,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    out: &mut Vec<FileEntry>,
) -> Result<(), String> {
    let mut children: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("Falha ao ler {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .collect();

    children.sort_by(|a, b| {
        let a_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);

        b_dir.cmp(&a_dir).then_with(|| {
            a.file_name()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.file_name().to_string_lossy().to_lowercase())
        })
    });

    for child in children {
        let path = child.path();
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        let name = child.file_name().to_string_lossy().to_string();
        let relative = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let size = if is_dir {
            0
        } else {
            child.metadata().map(|m| m.len()).unwrap_or(0)
        };

        out.push(FileEntry {
            path: path.clone(),
            name,
            relative,
            is_dir,
            depth,
            size,
        });

        if is_dir && expanded.contains(&path) {
            collect_entries(&path, base, depth + 1, expanded, out)?;
        }
    }

    Ok(())
}

fn refresh_file_list(
    list: &ListBox,
    scroll: &ScrolledWindow,
    status: &Label,
    rows: &Rc<RefCell<Vec<FileEntry>>>,
    expanded: &Rc<RefCell<HashSet<PathBuf>>>,
    root: &Path,
) -> Result<usize, String> {
    let mut entries = Vec::new();
    {
        let expanded_borrow = expanded.borrow();
        collect_entries(root, root, 0, &expanded_borrow, &mut entries)?;
    }

    let adjustment = scroll.vadjustment();
    let previous_scroll = adjustment.value();

    clear_list(list);

    for entry in &entries {
        list.append(&build_row(
            list, scroll, status, rows, expanded, root, entry,
        ));
    }

    let count = entries.len();
    *rows.borrow_mut() = entries;

    glib::idle_add_local_once(move || {
        adjustment.set_value(previous_scroll);
    });

    Ok(count)
}

fn build_row(
    list: &ListBox,
    scroll: &ScrolledWindow,
    status: &Label,
    rows: &Rc<RefCell<Vec<FileEntry>>>,
    expanded: &Rc<RefCell<HashSet<PathBuf>>>,
    root: &Path,
    entry: &FileEntry,
) -> ListBoxRow {
    let row = ListBoxRow::new();
    row.set_tooltip_text(Some(&entry.relative));

    let hbox = GtkBox::new(Orientation::Horizontal, 8);
    hbox.set_margin_top(3);
    hbox.set_margin_bottom(3);
    hbox.set_margin_start(4 + (entry.depth as i32) * 18);
    hbox.set_margin_end(8);

    let is_expanded = entry.is_dir && expanded.borrow().contains(&entry.path);

    let arrow = GtkBox::new(Orientation::Horizontal, 0);
    arrow.set_size_request(22, -1);
    if entry.is_dir {
        let arrow_icon = Image::from_icon_name(if is_expanded {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        });
        arrow_icon.set_pixel_size(12);

        let toggle = Button::new();
        toggle.set_child(Some(&arrow_icon));
        toggle.add_css_class("flat");
        toggle.set_valign(gtk::Align::Center);
        toggle.set_tooltip_text(Some(if is_expanded {
            "Recolher pasta"
        } else {
            "Expandir pasta"
        }));

        let list = list.clone();
        let scroll = scroll.clone();
        let status = status.clone();
        let rows = rows.clone();
        let expanded = expanded.clone();
        let root = root.to_path_buf();
        let path = entry.path.clone();
        toggle.connect_clicked(move |_| {
            toggle_expanded(&path, &expanded);
            if let Err(e) = refresh_file_list(&list, &scroll, &status, &rows, &expanded, &root) {
                status.set_text(&format!("Erro ao listar arquivos: {e}"));
            }
        });

        arrow.append(&toggle);
    }
    hbox.append(&arrow);

    let icon = if entry.is_dir {
        Image::from_icon_name(if is_expanded { "folder-open" } else { "folder" })
    } else {
        let (content_type, _) = gio::functions::content_type_guess(Some(&entry.path), &[]);
        Image::from_gicon(&gio::functions::content_type_get_icon(&content_type))
    };
    icon.set_pixel_size(18);
    icon.set_valign(gtk::Align::Center);
    hbox.append(&icon);

    let name_label = Label::new(Some(&entry.name));
    name_label.set_halign(gtk::Align::Start);
    name_label.set_hexpand(true);
    name_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

    if entry.is_dir {
        let attrs = gtk::pango::AttrList::new();
        attrs.insert(gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold));
        name_label.set_attributes(Some(&attrs));
    }

    hbox.append(&name_label);

    if !entry.is_dir {
        let size_label = Label::new(Some(&human_size(entry.size)));
        size_label.set_opacity(0.6);
        size_label.set_halign(gtk::Align::End);
        hbox.append(&size_label);
    }

    row.set_child(Some(&hbox));

    if entry.is_dir {
        let list = list.clone();
        let scroll = scroll.clone();
        let status = status.clone();
        let rows = rows.clone();
        let expanded = expanded.clone();
        let root = root.to_path_buf();
        let path = entry.path.clone();
        row.connect_activate(move |_| {
            toggle_expanded(&path, &expanded);
            if let Err(e) = refresh_file_list(&list, &scroll, &status, &rows, &expanded, &root) {
                status.set_text(&format!("Erro ao listar arquivos: {e}"));
            }
        });
    }

    row
}

fn toggle_expanded(path: &Path, expanded: &Rc<RefCell<HashSet<PathBuf>>>) {
    let mut set = expanded.borrow_mut();
    if !set.remove(path) {
        set.insert(path.to_path_buf());
    }
}

fn clear_list(list: &ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
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

    #[test]
    fn collect_entries_respects_expansion() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/f.txt"), "x").unwrap();
        std::fs::write(dir.path().join("z.txt"), "z").unwrap();

        let mut collapsed = Vec::new();
        collect_entries(dir.path(), dir.path(), 0, &HashSet::new(), &mut collapsed).unwrap();
        assert_eq!(collapsed.len(), 2);
        assert_eq!(collapsed[0].name, "a");
        assert!(collapsed[0].is_dir);

        let mut expanded_set = HashSet::new();
        expanded_set.insert(dir.path().join("a"));
        expanded_set.insert(dir.path().join("a/b"));

        let mut expanded_entries = Vec::new();
        collect_entries(
            dir.path(),
            dir.path(),
            0,
            &expanded_set,
            &mut expanded_entries,
        )
        .unwrap();

        assert_eq!(expanded_entries.len(), 4);
        assert_eq!(expanded_entries[2].name, "f.txt");
        assert_eq!(expanded_entries[2].depth, 2);
    }
}
