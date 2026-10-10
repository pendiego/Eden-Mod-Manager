use crate::catalog::{self, ModKind, UA};
use crate::emu::Kind;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use tauri::{AppHandle, Emitter, State};
use walkdir::WalkDir;

const TMP_PREFIX: &str = ".eden-mod-manager-tmp-";
const SMASH_TID: &str = "01006A800016E000";
const MK8D_TID: &str = "0100152000022000";
const MK8D_RAW_DIRS: [&str; 5] = ["Audio", "Course", "Driver", "Kart", "UI"];
const DISABLED_DIR: &str = ".eden-mod-manager-disabled";
pub(crate) const ARC_DIRS: &[&str] = &["fighter", "sound", "ui", "stream", "stream;", "stage", "effect", "camera", "assist", "item", "prebuilt;", "common"];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Destination {
    #[default]
    Emulator,
    Arcropolis,
    Save,
}

pub struct Root {
    key: String,
    name: String,
    files: Vec<(PathBuf, String)>,
    destination: Destination,
}

pub struct PendingInstall {
    tid: String,
    mod_id: String,
    version: Option<String>,
    tmp: PathBuf,
    roots: Vec<Root>,
}

#[derive(Default)]
pub struct Pending(Mutex<HashMap<String, PendingInstall>>);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RootInfo {
    key: String,
    name: String,
    file_count: usize,
    destination: Destination,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    token: String,
    roots: Vec<RootInfo>,
    is_save: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    /// emulador onde foi instalado (manifestos antigos, sem o campo, são do Eden)
    #[serde(default)]
    emu: Kind,
    #[serde(default)]
    destination: Destination,
    tid: String,
    folder: String,
    mod_id: String,
    root_key: String,
    name: String,
    version: Option<String>,
    installed_at: u64,
}

/// `Installed` + estado na config do emulador (não persistido no manifesto).
#[derive(Serialize)]
pub struct InstalledView {
    #[serde(flatten)]
    item: Installed,
    enabled: bool,
}

#[derive(Serialize, Clone)]
struct Progress {
    received: u64,
    total: Option<u64>,
}

// ---------- helpers ----------

pub(crate) fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn encode_path(p: &str) -> String {
    p.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let s: String = s.trim_end_matches(|c| c == '.' || c == ' ').chars().take(80).collect();
    let s = s.trim_end_matches(|c| c == '.' || c == ' ').to_string();
    if s.is_empty() { "mod".into() } else { s }
}

fn manifest_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = crate::app_dir(app, crate::Dir::Data)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("installed.json"))
}

fn load_manifest(app: &AppHandle) -> Result<Vec<Installed>, String> {
    let p = manifest_path(app)?;
    Ok(std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
}

fn save_manifest(app: &AppHandle, m: &[Installed]) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(m).map_err(|e| e.to_string())?;
    let path = manifest_path(app)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("Falha ao salvar manifesto: {e}")
        })
}

pub fn cleanup_tmp(load: &Path) {
    if let Ok(rd) = std::fs::read_dir(load) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// GET com `Range` a partir de `from` (0 = download inteiro).
async fn get(url: &str, from: u64) -> Result<reqwest::Response, String> {
    let mut req = catalog::HTTP.get(url).header("User-Agent", UA);
    if from > 0 {
        req = req.header("Range", format!("bytes={from}-"));
    }
    let resp = req.send().await.map_err(|e| format!("Falha ao baixar {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Falha ao baixar {url}: HTTP {}", resp.status()));
    }
    Ok(resp)
}

/// Se a conexão cair no meio, retoma de onde parou (até 3 vezes); servidor que ignora `Range` reinicia do zero.
pub(crate) async fn download(url: &str, dest: &Path, app: &AppHandle) -> Result<(), String> {
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let mut resp = get(url, 0).await?;
    let total = resp.content_length();
    let mut f = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let (mut received, mut last, mut retries) = (0u64, 0u64, 0);
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                f.write_all(&chunk).map_err(|e| e.to_string())?;
                received += chunk.len() as u64;
                if received - last >= 256 * 1024 {
                    last = received;
                    let _ = app.emit("download-progress", Progress { received, total });
                }
            }
            Ok(None) => break,
            Err(e) => {
                retries += 1;
                if retries > 3 {
                    return Err(format!("Falha ao baixar {url}: {e}"));
                }
                resp = get(url, received).await?;
                if received > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                    f.set_len(0).and_then(|_| f.seek(SeekFrom::Start(0))).map_err(|e| e.to_string())?;
                    (received, last) = (0, 0);
                }
            }
        }
    }
    let _ = app.emit("download-progress", Progress { received, total });
    Ok(())
}

fn unsafe_path(p: &Path) -> bool {
    p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_) | Component::RootDir))
        || p.to_string_lossy().replace('\\', "/").split('/').any(|s| s == ".." || s.contains(':'))
}

pub fn extract(archive: &Path, out: &Path) -> Result<(), String> {
    let ext = archive.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "zip" => {
            let f = std::fs::File::open(archive).map_err(|e| e.to_string())?;
            let mut z = zip::ZipArchive::new(f).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
            for n in 0..z.len() {
                let entry = z.by_index(n).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
                if unsafe_path(Path::new(entry.name())) || entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
                    return Err("Falha ao extrair zip: caminho inseguro".into());
                }
            }
            z.extract(out).map_err(|e| format!("Falha ao extrair zip: {e}"))
        }
        "7z" => sevenz_rust2::decompress_file(archive, out).map_err(|e| format!("Falha ao extrair 7z: {e}")),
        "rar" => {
            let mut ar = unrar::Archive::new(archive)
                .open_for_processing()
                .map_err(|e| format!("Falha ao abrir rar: {e}"))?;
            while let Some(h) = ar.read_header().map_err(|e| format!("Falha ao ler rar: {e}"))? {
                if unsafe_path(&h.entry().filename) {
                    return Err("Falha ao extrair rar: caminho inseguro".into());
                }
                ar = h.extract_with_base(out).map_err(|e| format!("Falha ao extrair rar: {e}"))?;
            }
            Ok(())
        }
        _ => Err("Formato não suportado".into()),
    }
}

fn stem(p: &str) -> String {
    let f = p.rsplit('/').next().unwrap_or(p);
    f.rsplit_once('.').map(|(s, _)| s).unwrap_or(f).to_string()
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry_res in WalkDir::new(src) {
        let entry = entry_res.map_err(|e| {
            let msg = e.to_string();
            e.into_io_error().unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, msg))
        })?;
        let rel = entry.path().strip_prefix(src)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn detect_save_files(dir: &Path, tid: &str) -> Option<Vec<(PathBuf, String)>> {
    let files: Vec<_> = WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let rel = e.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
            (e.path().to_path_buf(), rel)
        })
        .collect();

    if files.is_empty() {
        return None;
    }

    for (_, rel) in &files {
        let low = rel.to_lowercase();
        if low.split('/').any(|s| matches!(s, "romfs" | "exefs" | "cheats" | "romfslite" | "romfs_ext"))
            || low.contains("ultimate/mods")
        {
            return None;
        }
    }

    let is_smash = tid.eq_ignore_ascii_case(SMASH_TID);
    let mut is_save = false;

    for (_, rel) in &files {
        let low = rel.to_lowercase();
        let file_name = low.rsplit('/').next().unwrap_or(&low);
        if file_name == "system_data.bin"
            || file_name == "userdata.dat"
            || file_name == "savedata.bin"
            || file_name == "savedata"
            || file_name == "save.dat"
            || file_name == "game_data.sav"
            || file_name == "progress.sav"
            || file_name.ends_with(".sav")
            || file_name == "main"
            || file_name == "backup"
            || low.contains("save_data/")
            || low.contains("savedata/")
        {
            is_save = true;
            break;
        }
    }

    if !is_save {
        return None;
    }

    let mut out = Vec::new();
    let tid_upper = tid.to_ascii_uppercase();
    let tid_lower = tid.to_ascii_lowercase();

    for (path, rel) in files {
        if unsafe_path(Path::new(&rel)) {
            continue;
        }
        let dest = if is_smash {
            if let Some(i) = rel.find("save_data/") {
                rel[i..].to_string()
            } else if let Some(stripped) = rel.strip_prefix(&format!("{tid_upper}/"))
                .or_else(|| rel.strip_prefix(&format!("{tid_lower}/")))
            {
                if stripped.starts_with("save_data/") {
                    stripped.to_string()
                } else {
                    format!("save_data/{stripped}")
                }
            } else {
                let segs: Vec<&str> = rel.split('/').collect();
                let inner = if segs.len() > 1 && !matches!(segs[0].to_lowercase().as_str(), "mii" | "spirits" | "stage") {
                    segs[1..].join("/")
                } else {
                    rel.clone()
                };
                if inner.starts_with("save_data/") {
                    inner
                } else {
                    format!("save_data/{inner}")
                }
            }
        } else {
            if let Some(stripped) = rel.strip_prefix(&format!("{tid_upper}/"))
                .or_else(|| rel.strip_prefix(&format!("{tid_lower}/")))
            {
                stripped.to_string()
            } else {
                let segs: Vec<&str> = rel.split('/').collect();
                if segs.len() > 1 && segs[0].to_lowercase().contains("save") {
                    segs[1..].join("/")
                } else {
                    rel.clone()
                }
            }
        };

        out.push((path, dest));
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn roots_from_extracted(dir: &Path, fallback: &str, tid: &str) -> Result<Vec<Root>, String> {
    let files: Vec<_> = WalkDir::new(dir).into_iter().filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let rel = e.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
            (e.path().to_path_buf(), rel)
        }).collect();
    let mut arc_roots = std::collections::BTreeSet::new();
    let mut wrapped_roots = std::collections::BTreeSet::new();
    let mut has_mod_root = false;
    for (_, rel) in &files {
        let segs: Vec<_> = rel.split('/').collect();
        if let Some(i) = segs.windows(2).position(|s| s == ["ultimate", "mods"]) {
            if segs.len() > i + 3 {
                let root = segs[..i + 3].join("/");
                wrapped_roots.insert(root.clone());
                arc_roots.insert(root);
            }
        } else {
            let mod_root = catalog::mod_root(rel);
            has_mod_root |= mod_root.is_some();
            if mod_root.is_none() {
                if let Some(i) = segs[..segs.len().saturating_sub(1)].iter()
                    .position(|s| ARC_DIRS.contains(s)) {
                    arc_roots.insert(segs[..i].join("/"));
                }
            }
        }
    }
    // Um diretório de conteúdo dentro de outro mod não inicia uma segunda variante.
    arc_roots = arc_roots.iter().filter(|root| wrapped_roots.contains(*root) || !arc_roots.iter().any(|parent|
        parent != *root && (parent.is_empty() || root.starts_with(&format!("{parent}/"))))).cloned().collect();
    // O MK8D recebe `Audio/`, `Course/`, `Driver/`, `Kart/` e `UI/` sem a pasta `romfs/`.
    let raw_mk8d = tid.eq_ignore_ascii_case(MK8D_TID) && !has_mod_root && wrapped_roots.is_empty();
    let smash = tid.eq_ignore_ascii_case(SMASH_TID);
    let mut groups: BTreeMap<(String, bool), Vec<(PathBuf, String)>> = BTreeMap::new();
    for (path, rel) in files {
        if unsafe_path(Path::new(&rel)) { continue; }
        let arc = arc_roots.iter().filter_map(|root| {
            if root.is_empty() { Some((root, rel.as_str())) }
            else { rel.strip_prefix(root.as_str()).and_then(|s| s.strip_prefix('/')).map(|s| (root, s)) }
        }).max_by_key(|(root, _)| root.len());
        let ordinary = catalog::mod_root(&rel).filter(|(_, d)| !unsafe_path(Path::new(d)));
        let ordinary_tree = rel.split('/').any(|s| ["romfs", "exefs", "cheats"].iter().any(|d| s.eq_ignore_ascii_case(d)));
        if smash {
            if let Some((root, dest)) = arc.filter(|(root, _)| wrapped_roots.contains(*root) || !ordinary_tree) {
                groups.entry((root.clone(), true)).or_default().push((path, dest.to_string()));
                continue;
            }
        }
        if let Some((root, dest)) = ordinary {
            groups.entry((root, false)).or_default().push((path, dest));
        } else if raw_mk8d
            && rel.split_once('/').is_some_and(|(root, _)| MK8D_RAW_DIRS.iter().any(|d| root.eq_ignore_ascii_case(d)))
        {
            groups.entry((String::new(), false)).or_default().push((path, format!("romfs/{rel}")));
        }
    }
    if !wrapped_roots.is_empty() && !smash {
        return Err("ARCropolis requer Super Smash Bros. Ultimate.".into());
    }
    if groups.is_empty() {
        if let Some(save_files) = detect_save_files(dir, tid) {
            return Ok(vec![Root {
                name: format!("Save: {fallback}"),
                key: format!("save:{fallback}"),
                files: save_files,
                destination: Destination::Save,
            }]);
        }
    }
    Ok(groups.into_iter().map(|((key, arc), files)| Root {
        name: catalog::display_name(&key, fallback), key: if arc { format!("arcropolis:{key}") } else { key }, files,
        destination: if arc { Destination::Arcropolis } else { Destination::Emulator },
    }).collect())
}

async fn do_prepare(app: &AppHandle, m: &catalog::ModEntry, tmp: &Path, tid: &str) -> Result<Vec<Root>, String> {
    let url = |src: &str| m.source.raw_url(src);
    match m.kind {
        ModKind::Files => {
            let mut files = Vec::new();
            for f in &m.files {
                let dest = checked_path(tmp, &f.dest)?;
                download(&url(&f.src), &dest, app).await?;
                files.push((dest, f.dest.clone()));
            }
            Ok(vec![Root { key: String::new(), name: m.name.clone(), files, destination: Destination::Emulator }])
        }
        ModKind::Archive => {
            let src = &m.files[0].src;
            let (url, ext, name) = if m.source == catalog::Source::Gamebanana {
                let (u, e) = crate::gamebanana::download_url(src).await?;
                (u, e, m.name.clone())
            } else {
                (url(src), src.rsplit('.').next().unwrap_or("").to_lowercase(), stem(src))
            };
            let arc = tmp.join(format!("_archive.{ext}"));
            download(&url, &arc, app).await?;
            let x = tmp.join("x");
            let xx = x.clone();
            tauri::async_runtime::spawn_blocking(move || extract(&arc, &xx))
                .await
                .map_err(|e| e.to_string())??;
            roots_from_extracted(&x, &name, tid)
        }
        ModKind::Pack => {
            let (url, pack_tid, size) = (m.source.raw_url(crate::pack::ZIP), m.files[0].src.clone(), m.size);
            let x = tmp.join("x");
            let (xx, app2) = (x.clone(), app.clone());
            tauri::async_runtime::spawn_blocking(move || {
                crate::pack::extract(&url, &pack_tid, size, &xx, |received, total| {
                    let _ = app2.emit("download-progress", Progress { received, total: Some(total) });
                })
            })
                .await
                .map_err(|e| e.to_string())??;
            roots_from_extracted(&x, &m.name, tid)
        }
    }
}

// ---------- commands ----------

#[tauri::command]
pub async fn prepare_install(
    app: AppHandle,
    cat: State<'_, crate::CatalogState>,
    pending: State<'_, Pending>,
    tid: String,
    mod_id: String,
) -> Result<Prepared, String> {
    crate::emu::check_tid(&tid)?;
    let m = cat
        .0
        .lock()
        .as_ref()
        .and_then(|c| c.mods.iter().find(|m| m.id == mod_id).cloned())
        .or_else(|| crate::gamebanana::find_cached_mod(&mod_id))
        .ok_or("Mod não encontrado no catálogo")?;
    if m.tid.as_deref().is_some_and(|t| !t.eq_ignore_ascii_case(&tid)) {
        return Err("Este mod é de outro jogo".into());
    }
    let (tmp, token) = new_tmp(&app)?;
    let r = do_prepare(&app, &m, &tmp, &tid).await;
    stage(&pending, tid, mod_id, m.version.clone(), tmp, token, r)
}

fn new_tmp(app: &AppHandle) -> Result<(PathBuf, String), String> {
    let load = crate::resolve_emu(app)?.mods_dir();
    let token = format!(
        "{:x}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    );
    let tmp = load.join(format!("{TMP_PREFIX}{token}"));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("Falha ao criar pasta temporária: {e}"))?;
    Ok((tmp, token))
}

/// Registra a instalação pendente, ou apaga a pasta temporária se a preparação falhou.
fn stage(
    pending: &Pending,
    tid: String,
    mod_id: String,
    version: Option<String>,
    tmp: PathBuf,
    token: String,
    roots: Result<Vec<Root>, String>,
) -> Result<Prepared, String> {
    let roots = match roots {
        Ok(r) if !r.is_empty() => r,
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err("Layout do mod não reconhecido. Abra a página do mod.".into());
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    let is_save = roots.iter().any(|r| r.destination == Destination::Save);
    let infos = roots
        .iter()
        .map(|r| RootInfo {
            key: r.key.clone(),
            name: r.name.clone(),
            file_count: r.files.len(),
            destination: r.destination,
        })
        .collect();
    pending.0.lock().insert(token.clone(), PendingInstall { tid, mod_id, version, tmp, roots });
    Ok(Prepared { token, roots: infos, is_save })
}

/// Mod baixado pelo usuário (zip/7z/rar): extrai e segue o mesmo fluxo de escolha de variantes e `commit`.
#[tauri::command]
pub async fn prepare_local(app: AppHandle, pending: State<'_, Pending>, tid: String, path: String) -> Result<Prepared, String> {
    crate::emu::check_tid(&tid)?;
    let src = PathBuf::from(&path);
    let file = src.file_name().ok_or("Arquivo inválido")?.to_string_lossy().into_owned();
    let (tmp, token) = new_tmp(&app)?;
    let (arc, out) = (src, tmp.join("x"));
    let name = stem(&file);
    let selected_tid = tid.clone();
    let r = tauri::async_runtime::spawn_blocking(move || extract(&arc, &out).and_then(|_| roots_from_extracted(&out, &name, &selected_tid)))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    stage(&pending, tid, format!("local:{file}"), None, tmp, token, r)
}

fn destination_dir(emu: &crate::emu::Emu, tid: &str, destination: Destination, disabled: bool) -> Result<PathBuf, String> {
    crate::emu::check_tid(tid)?;
    if destination == Destination::Emulator { return Ok(emu.tid_dir(tid)); }
    if destination == Destination::Save { return emu.save_dir(tid); }
    if !tid.eq_ignore_ascii_case(SMASH_TID) { return Err("ARCropolis requer Super Smash Bros. Ultimate.".into()); }
    if emu.kind == Kind::Yuzu { return Err("ARCropolis não é compatível com este emulador.".into()); }
    Ok(emu.sd_dir().join("ultimate").join(if disabled { DISABLED_DIR } else { "mods" }))
}

fn check_arcropolis(emu: &crate::emu::Emu, tid: &str) -> Result<(), String> {
    destination_dir(emu, tid, Destination::Arcropolis, false)?;
    let framework = emu.sd_dir().join("atmosphere/contents").join(SMASH_TID);
    let missing: Vec<_> = [ARC_MARKER, SKYLINE_MARKER, NPDM_MARKER]
        .into_iter().map(|p| framework.join(p)).filter(|p| !p.is_file()).collect();
    if missing.is_empty() { Ok(()) } else {
        Err(format!("Skyline/ARCropolis não encontrados na SD emulada: {}",
            missing.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")))
    }
}

const ARC_MARKER: &str = "romfs/skyline/plugins/libarcropolis.nro";
const SKYLINE_MARKER: &str = "exefs/subsdk9";
const NPDM_MARKER: &str = "exefs/main.npdm";
/// `main.npdm` antes de `subsdk9`: o `subsdk9` é o último a aparecer na instalação.
const SKYLINE_FILES: [&str; 2] = [NPDM_MARKER, SKYLINE_MARKER];

const SKYLINE_URL: &str = "https://github.com/skyline-dev/skyline/releases/latest/download/skyline.zip";
const ARCROPOLIS_URL: &str = "https://github.com/Raytwo/ARCropolis/releases/latest/download/release.zip";

/// Copia do zip a primeira entrada cujo caminho (minúsculo, com `/`) termina em `sufixo` para `dest/<destino>`.
/// Normaliza a caixa do TID: o destino é sempre o caminho canônico, não o do zip.
fn framework_files(zip: &Path, files: &[(&str, &str)], dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
    let names: Vec<(bool, String)> = (0..archive.len())
        .map(|i| archive.by_index_raw(i).map(|f| (f.is_dir(), f.name().replace('\\', "/").to_ascii_lowercase())))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Falha ao extrair zip: {e}"))?;
    let mut found = Vec::new();
    for (suffix, rel) in files {
        let i = names.iter().position(|(dir, n)| !dir && n.ends_with(suffix))
            .ok_or("Pacote do Skyline/ARCropolis em formato inesperado")?;
        found.push((i, *rel));
    }
    for (i, rel) in found {
        let mut entry = archive.by_index(i).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
        copy_entry(&mut entry, &dest.join(rel))?;
    }
    Ok(())
}

/// Grava a entrada em `target` via `.part` + rename, para nunca deixar arquivo pela metade.
fn copy_entry(entry: &mut impl std::io::Read, target: &Path) -> Result<(), String> {
    let mut name = target.file_name().unwrap().to_os_string();
    name.push(".part");
    let part = target.with_file_name(name);
    let copied = std::fs::create_dir_all(target.parent().unwrap())
        .and_then(|_| std::fs::File::create(&part))
        .and_then(|mut f| std::io::copy(entry, &mut f).map(|_| ()))
        .and_then(|_| std::fs::rename(&part, target));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&part);
        return Err(format!("Falha ao mover arquivo: {e}"));
    }
    Ok(())
}

/// O release.zip do ARCropolis também traz `ultimate/arcropolis/` (layouts do menu e config) na raiz da SD.
/// Copia só o que ainda não existe, para não sobrescrever configuração do usuário.
fn arcropolis_resources(zip: &Path, sd: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("Falha ao extrair zip: {e}"))?;
        let Some(rel) = entry.enclosed_name() else { continue };
        if entry.is_dir() || !rel.starts_with("ultimate/arcropolis") { continue; }
        let target = sd.join(rel);
        if !target.exists() { copy_entry(&mut entry, &target)?; }
    }
    Ok(())
}

/// Baixa o zip em uma pasta temporária, entrega o caminho a `install` e apaga a pasta.
async fn with_package(app: &AppHandle, url: &str, install: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    let (tmp, _) = new_tmp(app)?;
    let zip = tmp.join("framework.zip");
    let r = match download(url, &zip, app).await {
        Ok(()) => install(&zip),
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

/// Plugin do Skyline dentro de um mod (`romfs/skyline/plugins/*.nro`): o jogo precisa do Skyline.
fn is_skyline_plugin(rel: &str) -> bool {
    let l = rel.replace('\\', "/").to_ascii_lowercase();
    l.starts_with("romfs/skyline/plugins/") && l.ends_with(".nro")
}

/// `mod_dir` é a pasta de um mod já instalado (`<load>/<tid>/<mod>`).
fn has_skyline_plugins(mod_dir: &Path) -> bool {
    std::fs::read_dir(mod_dir.join("romfs/skyline/plugins")).is_ok_and(|rd| rd.flatten()
        .any(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("nro"))))
}

/// O `main.npdm` do Skyline traz o TID do Smash no ACI0 (offset 0x340); outros jogos precisam do próprio TID.
fn patch_npdm_tid(path: &Path, tid: &str) -> Result<(), String> {
    let id = u64::from_str_radix(tid, 16).map_err(|_| "TID inválido".to_string())?;
    let mut b = std::fs::read(path).map_err(|e| format!("Falha ao mover arquivo: {e}"))?;
    if b.len() < 0x348 || &b[0x330..0x334] != b"ACI0" {
        return Err("Pacote do Skyline/ARCropolis em formato inesperado".into());
    }
    b[0x340..0x348].copy_from_slice(&id.to_le_bytes());
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    std::fs::write(&part, &b).and_then(|_| std::fs::rename(&part, path)).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("Falha ao mover arquivo: {e}")
    })
}

/// `<SD>/atmosphere/contents/<TID>`: onde ficam o Skyline (`exefs/`) e, no Smash, o ARCropolis.
fn framework_dir(emu: &crate::emu::Emu, tid: &str) -> PathBuf {
    emu.sd_dir().join("atmosphere/contents").join(tid.to_ascii_uppercase())
}

/// Instala o que falta do Skyline do jogo `tid`. Nunca sobrescreve `subsdk9` nem um `main.npdm` já existente.
pub(crate) async fn ensure_skyline(app: &AppHandle, tid: &str) -> Result<(), String> {
    crate::emu::check_tid(tid)?;
    let emu = crate::resolve_emu(app)?;
    let dir = framework_dir(&emu, tid);
    if SKYLINE_FILES.iter().all(|p| dir.join(p).is_file()) { return Ok(()); }
    with_package(app, SKYLINE_URL, |zip| skyline_from_zip(zip, &dir, tid)).await
}

/// Extrai e valida (NPDM com o TID do jogo) numa pasta ao lado do zip e só então publica em `dir`:
/// `main.npdm` primeiro e `subsdk9` por último, para um erro no meio nunca deixar o Skyline "instalado" pela metade.
fn skyline_from_zip(zip: &Path, dir: &Path, tid: &str) -> Result<(), String> {
    let missing: Vec<_> = SKYLINE_FILES.iter().copied().filter(|p| !dir.join(p).is_file()).collect();
    let stage = zip.with_file_name("stage");
    let files: Vec<_> = missing.iter().map(|p| (*p, *p)).collect();
    framework_files(zip, &files, &stage)?;
    if missing.contains(&NPDM_MARKER) { patch_npdm_tid(&stage.join(NPDM_MARKER), tid)?; }
    for rel in missing {
        let mut f = std::fs::File::open(stage.join(rel)).map_err(|e| format!("Falha ao mover arquivo: {e}"))?;
        copy_entry(&mut f, &dir.join(rel))?;
    }
    Ok(())
}

/// Smash: Skyline + ARCropolis. Baixa e instala o que faltar na SD emulada, sem sobrescrever arquivos presentes.
pub(crate) async fn ensure_arcropolis(app: &AppHandle, tid: &str) -> Result<(), String> {
    let emu = crate::resolve_emu(app)?;
    destination_dir(&emu, tid, Destination::Arcropolis, false)?;
    let dir = framework_dir(&emu, SMASH_TID);
    if !dir.join(ARC_MARKER).is_file() {
        with_package(app, ARCROPOLIS_URL, |zip| {
            arcropolis_resources(zip, &emu.sd_dir())?;
            framework_files(zip, &[("skyline/plugins/libarcropolis.nro", ARC_MARKER)], &dir)
        }).await?;
    }
    ensure_skyline(app, SMASH_TID).await?;
    check_arcropolis(&emu, tid)
}

/// Instala o que o jogo precisa: Skyline + ARCropolis no Smash, só o Skyline nos demais.
#[tauri::command]
pub async fn install_frameworks(app: AppHandle, tid: String) -> Result<(), String> {
    crate::emu::check_tid(&tid)?;
    if tid.eq_ignore_ascii_case(SMASH_TID) { ensure_arcropolis(&app, &tid).await } else { ensure_skyline(&app, &tid).await }
}

#[derive(Serialize)]
pub struct FrameworkStatus {
    /// O jogo precisa do Skyline: é o Smash ou tem mod com plugin do Skyline ativo.
    needed: bool,
    skyline: bool,
    /// `None` fora do Smash (ou no yuzu, que não suporta ARCropolis).
    arcropolis: Option<bool>,
}

/// Detecta o que já está instalado (só leitura, sem rede).
#[tauri::command]
pub fn framework_status(app: AppHandle, tid: String) -> Result<FrameworkStatus, String> {
    crate::emu::check_tid(&tid)?;
    let emu = crate::resolve_emu(&app)?;
    let dir = framework_dir(&emu, &tid);
    let smash = tid.eq_ignore_ascii_case(SMASH_TID);
    let off = crate::addons::disabled(&emu, &tid);
    let plugins = std::fs::read_dir(emu.tid_dir(&tid)).is_ok_and(|rd| rd.flatten().any(|e|
        !off.contains(&e.file_name().to_string_lossy().into_owned()) && has_skyline_plugins(&e.path())));
    Ok(FrameworkStatus {
        needed: smash || plugins,
        skyline: SKYLINE_FILES.iter().all(|p| dir.join(p).is_file()),
        arcropolis: (smash && emu.kind != Kind::Yuzu).then(|| dir.join(ARC_MARKER).is_file()),
    })
}

/// A raiz configurada pode ser um link/junction; os caminhos do mod abaixo dela não.
fn checked_path(base: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel = Path::new(relative);
    if relative.is_empty() || unsafe_path(rel) || relative.split(['/', '\\']).any(|s| s == "." || s == ".." || s.is_empty()) {
        return Err("Pasta não foi instalada pelo Eden Mod Manager".into());
    }
    let target = base.join(rel);
    for parent in target.ancestors().take_while(|p| *p != base) {
        if let Ok(meta) = std::fs::symlink_metadata(parent) {
            if meta.file_type().is_symlink() {
                return Err("Pasta não foi instalada pelo Eden Mod Manager".into());
            }
        }
    }
    Ok(target)
}

fn item_paths(emu: &crate::emu::Emu, item: &Installed) -> Result<(PathBuf, Option<PathBuf>), String> {
    if item.destination == Destination::Save {
        let save = destination_dir(emu, &item.tid, Destination::Save, false)?;
        return Ok((save, None));
    }
    if item.folder.contains(['/', '\\']) { return Err("Pasta não foi instalada pelo Eden Mod Manager".into()); }
    let on = checked_path(&destination_dir(emu, &item.tid, item.destination, false)?, &item.folder)?;
    let off = if item.destination == Destination::Arcropolis {
        Some(checked_path(&destination_dir(emu, &item.tid, item.destination, true)?, &item.folder)?)
    } else { None };
    Ok((on, off))
}

fn item_location(emu: &crate::emu::Emu, item: &Installed) -> Result<(PathBuf, bool), String> {
    let (on, off) = item_paths(emu, item)?;
    if let Some(off) = off {
        // Duas pastas não podem ser atribuídas ao mesmo registro com segurança.
        if on.exists() && off.exists() { return Err(format!("Pasta de destino já existe: {}", on.display())); }
        if off.is_dir() { return Ok((off, false)); }
    }
    Ok((on, true))
}

fn toggle_arcropolis(emu: &crate::emu::Emu, item: &Installed, enabled: bool) -> Result<(), String> {
    let (source, current) = item_location(emu, item)?;
    if current == enabled { return Ok(()); }
    if enabled { check_arcropolis(emu, &item.tid)?; }
    let (on, off) = item_paths(emu, item)?;
    let target = if enabled { on } else { off.unwrap() };
    if target.exists() { return Err(format!("Pasta de destino já existe: {}", target.display())); }
    std::fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::rename(source, target).map_err(|e| format!("Falha ao mover arquivo: {e}"))
}

fn commit(app: &AppHandle, p: PendingInstall, keys: &[String]) -> Result<Vec<Installed>, String> {
    let emu = crate::resolve_emu(app)?;
    let mut manifest = load_manifest(app)?;
    let selected: Vec<_> = p.roots.iter().filter(|r| keys.contains(&r.key)).collect();
    if selected.iter().any(|r| r.destination == Destination::Arcropolis) { check_arcropolis(&emu, &p.tid)?; }
    let mut done = Vec::new();
    let mut created = Vec::new();
    let mut backups = Vec::new();
    let mut save_backups = Vec::new();
    let result = (|| {
        for (n, root) in selected.into_iter().enumerate() {
            if root.destination == Destination::Save {
                if emu.is_running() {
                    return Err("Feche o emulador antes de instalar um save game.".into());
                }
                let game_dir = destination_dir(&emu, &p.tid, Destination::Save, false)?;
                let save_parent = game_dir.parent().ok_or("Pasta pai de save inválida")?;
                let staging = checked_path(save_parent, &format!("{TMP_PREFIX}save-stage-{}-{n}", p.tmp.file_name().unwrap().to_string_lossy()))?;
                std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
                created.push(staging.clone());

                for (src, dest) in &root.files {
                    let to = checked_path(&staging, dest)?;
                    if let Some(d) = to.parent() { std::fs::create_dir_all(d).map_err(|e| e.to_string())?; }
                    std::fs::copy(src, &to).map_err(|e| format!("Falha ao copiar arquivo de save: {e}"))?;
                }

                let has_files = game_dir.is_dir()
                    && WalkDir::new(&game_dir).into_iter().filter_map(Result::ok).any(|e| e.file_type().is_file());
                if has_files {
                    let base_name = format!("{}_backup_{}", p.tid.to_ascii_uppercase(), catalog::now_secs());
                    let unique_name = unique_folder(save_parent, &base_name);
                    let backup_dir = save_parent.join(&unique_name);
                    copy_dir_all(&game_dir, &backup_dir)
                        .map_err(|e| format!("Falha ao criar backup do save: {e}"))?;
                    save_backups.push((game_dir.clone(), backup_dir));
                }

                std::fs::create_dir_all(&game_dir).map_err(|e| e.to_string())?;
                copy_dir_all(&staging, &game_dir).map_err(|e| format!("Falha ao aplicar save: {e}"))?;

                done.push(Installed {
                    emu: emu.kind, destination: root.destination, tid: p.tid.clone(),
                    folder: format!("save-{}", catalog::now_secs()),
                    mod_id: p.mod_id.clone(), root_key: root.key.clone(), name: root.name.clone(),
                    version: p.version.clone(), installed_at: catalog::now_secs(),
                });
                continue;
            }
            let reinstall = manifest.iter().position(|i| i.emu == emu.kind && i.tid == p.tid
                && i.destination == root.destination && i.mod_id == p.mod_id && i.root_key == root.key);
            let old = reinstall.map(|pos| item_location(&emu, &manifest[pos])).transpose()?;
            let game_dir = destination_dir(&emu, &p.tid, root.destination, old.as_ref().is_some_and(|(_, on)| !on))?;
            let disabled_dir = destination_dir(&emu, &p.tid, root.destination, true)?;
            let base = sanitize(&root.name);
            let folder = if let Some(pos) = reinstall { manifest[pos].folder.clone() } else {
                let mut folder = unique_folder_pair(&game_dir, &disabled_dir, &base);
                let mut suffix = 2;
                while manifest.iter().any(|i| i.emu == emu.kind && i.tid == p.tid && i.folder == folder)
                    || emu.tid_dir(&p.tid).join(&folder).exists() {
                    folder = unique_folder_pair(&game_dir, &disabled_dir, &format!("{base} ({suffix})"));
                    suffix += 1;
                }
                folder
            };
            let target = checked_path(&game_dir, &folder)?;
            let staging = checked_path(&game_dir, &format!("{TMP_PREFIX}{}-{n}", p.tmp.file_name().unwrap().to_string_lossy()))?;
            std::fs::create_dir_all(&game_dir).map_err(|e| e.to_string())?;
            std::fs::create_dir(&staging).map_err(|e| e.to_string())?;
            created.push(staging.clone());
            for (src, dest) in &root.files {
                let to = checked_path(&staging, dest)?;
                if std::fs::symlink_metadata(src).map_err(|e| e.to_string())?.file_type().is_symlink() {
                    return Err("Pasta não foi instalada pelo Eden Mod Manager".into());
                }
                if let Some(d) = to.parent() { std::fs::create_dir_all(d).map_err(|e| e.to_string())?; }
                std::fs::copy(src, &to).map_err(|e| format!("Falha ao mover arquivo: {e}"))?;
            }
            if let Some((old, _)) = old {
                if old.exists() {
                    let backup = checked_path(old.parent().unwrap(), &format!("{TMP_PREFIX}backup-{}-{n}", p.tmp.file_name().unwrap().to_string_lossy()))?;
                    if backup.exists() { return Err(format!("Pasta de destino já existe: {}", backup.display())); }
                    std::fs::rename(&old, &backup).map_err(|e| format!("Falha ao mover arquivo: {e}"))?;
                    backups.push((old, backup));
                }
            }
            if target.exists() { return Err(format!("Pasta de destino já existe: {}", target.display())); }
            std::fs::rename(&staging, &target).map_err(|e| format!("Falha ao mover arquivo: {e}"))?;
            created.push(target);
            if let Some(pos) = reinstall { manifest.remove(pos); }
            done.push(Installed {
                emu: emu.kind, destination: root.destination, tid: p.tid.clone(), folder,
                mod_id: p.mod_id.clone(), root_key: root.key.clone(), name: root.name.clone(),
                version: p.version.clone(), installed_at: catalog::now_secs(),
            });
        }
        manifest.extend(done.iter().cloned());
        save_manifest(app, &manifest)
    })();
    if let Err(e) = result {
        for target in created.iter().rev() { let _ = std::fs::remove_dir_all(target); }
        for (old, backup) in backups.iter().rev() { let _ = std::fs::rename(backup, old); }
        for (live, backup) in save_backups.iter().rev() {
            let _ = std::fs::remove_dir_all(live);
            let _ = copy_dir_all(backup, live);
        }
        return Err(e);
    }
    for (_, backup) in backups { let _ = std::fs::remove_dir_all(backup); }
    Ok(done)
}

/// `base`, ou `base (2)`, `base (3)`… o primeiro que ainda não existe em `dir`.
fn unique_folder(dir: &Path, base: &str) -> String {
    let mut folder = base.to_string();
    let mut n = 2;
    while dir.join(&folder).exists() {
        folder = format!("{base} ({n})");
        n += 1;
    }
    folder
}

fn unique_folder_pair(dir: &Path, disabled: &Path, base: &str) -> String {
    let mut folder = unique_folder(dir, base);
    let mut n = 2;
    while dir.join(&folder).exists() || disabled.join(&folder).exists() {
        folder = format!("{base} ({n})");
        n += 1;
    }
    folder
}

#[tauri::command]
pub async fn commit_install(
    app: AppHandle,
    pending: State<'_, Pending>,
    token: String,
    keys: Vec<String>,
) -> Result<Vec<Installed>, String> {
    let p = pending.0.lock().remove(&token).ok_or("Instalação pendente não encontrada")?;
    let tmp = p.tmp.clone();
    let selected = |r: &&Root| keys.contains(&r.key);
    let arc = p.roots.iter().filter(selected).any(|r| r.destination == Destination::Arcropolis);
    let sky = p.roots.iter().filter(selected).any(|r| r.files.iter().any(|(_, rel)| is_skyline_plugin(rel)));
    let ensured = if arc { ensure_arcropolis(&app, &p.tid).await } else if sky { ensure_skyline(&app, &p.tid).await } else { Ok(()) };
    let r = match ensured {
        Ok(()) => commit(&app, p, &keys),
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(tmp);
    r
}

#[tauri::command]
pub fn cancel_install(pending: State<'_, Pending>, token: String) {
    if let Some(p) = pending.0.lock().remove(&token) {
        let _ = std::fs::remove_dir_all(p.tmp);
    }
}

#[tauri::command]
pub fn list_installed(app: AppHandle, tid: String) -> Result<Vec<InstalledView>, String> {
    crate::emu::check_tid(&tid)?;
    let emu = crate::resolve_emu(&app)?;
    let all = load_manifest(&app)?;
    let mut kept = Vec::new();
    for item in &all {
        if item.emu != emu.kind || item.tid != tid || item_location(&emu, item).map(|(p, _)| p.is_dir()).unwrap_or(true) {
            kept.push(item.clone());
        }
    }
    if kept.len() != all.len() {
        save_manifest(&app, &kept)?;
    }
    let off = crate::addons::disabled(&emu, &tid);
    Ok(kept
        .into_iter()
        .filter(|i| i.emu == emu.kind && i.tid == tid)
        .map(|item| {
            let enabled = if item.destination == Destination::Arcropolis {
                item_location(&emu, &item).map(|(_, on)| on).unwrap_or(false)
            } else if item.destination == Destination::Save {
                true
            } else { !off.contains(&item.folder) };
            InstalledView { enabled, item }
        })
        .collect())
}

pub(crate) fn mod_folder(app: &AppHandle, tid: &str) -> Result<PathBuf, String> {
    crate::emu::check_tid(tid)?;
    let emu = crate::resolve_emu(app)?;
    if load_manifest(app)?.iter().any(|i| i.emu == emu.kind && i.tid == tid
        && i.destination == Destination::Arcropolis && item_paths(&emu, i)
            .is_ok_and(|(on, off)| on.is_dir() || off.is_some_and(|p| p.is_dir()))) {
        destination_dir(&emu, tid, Destination::Arcropolis, false)
    } else { Ok(emu.tid_dir(tid)) }
}

#[tauri::command]
pub async fn set_mod_enabled(app: AppHandle, tid: String, folder: String, enabled: bool) -> Result<(), String> {
    crate::emu::check_tid(&tid)?;
    let emu = crate::resolve_emu(&app)?;
    let manifest = load_manifest(&app)?;
    let item = manifest.iter().find(|i| i.emu == emu.kind && i.tid == tid && i.folder == folder)
        .cloned()
        .ok_or("Pasta não foi instalada pelo Eden Mod Manager")?;
    item_paths(&emu, &item)?;
    if item.destination == Destination::Save {
        return Err("Saves de jogos não podem ser desativados".into());
    }
    if item.destination == Destination::Arcropolis {
        if enabled { ensure_arcropolis(&app, &tid).await?; }
        toggle_arcropolis(&emu, &item, enabled)
    } else {
        if enabled && has_skyline_plugins(&emu.tid_dir(&tid).join(&folder)) { ensure_skyline(&app, &tid).await?; }
        crate::addons::set_enabled(&emu, &tid, &folder, enabled)
    }
}

#[tauri::command]
pub async fn list_conflicts(app: AppHandle, tid: String) -> Result<Vec<crate::addons::Conflict>, String> {
    crate::emu::check_tid(&tid)?;
    let emu = crate::resolve_emu(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut conflicts = crate::addons::conflicts(&emu.tid_dir(&tid), &crate::addons::disabled(&emu, &tid));
        if tid.eq_ignore_ascii_case(SMASH_TID) && emu.kind != Kind::Yuzu {
            conflicts.extend(crate::addons::arcropolis_conflicts(&emu.sd_dir().join("ultimate/mods")));
        }
        conflicts
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn uninstall(app: AppHandle, tid: String, folder: String) -> Result<(), String> {
    crate::emu::check_tid(&tid)?;
    let mut m = load_manifest(&app)?;
    let emu = crate::resolve_emu(&app)?;
    let pos = m
        .iter()
        .position(|i| i.emu == emu.kind && i.tid == tid && i.folder == folder)
        .ok_or("Pasta não foi instalada pelo Eden Mod Manager")?;
    if m[pos].destination != Destination::Save {
        let (target, _) = item_location(&emu, &m[pos])?;
        match std::fs::remove_dir_all(target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Falha ao remover: {e}")),
        }
        if m[pos].destination == Destination::Emulator && crate::addons::disabled(&emu, &tid).contains(&folder) {
            let _ = crate::addons::set_enabled(&emu, &tid, &folder, true);
        }
    }
    m.remove(pos);
    save_manifest(&app, &m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("emm-arc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, rel: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"payload").unwrap();
    }

    #[test]
    fn legacy_manifest_routes_to_normal_mods_and_new_manifest_roundtrips() {
        let legacy = r#"{"tid":"01006A800016E000","folder":"FPS","modId":"fps","rootKey":"","name":"FPS","version":null,"installedAt":1}"#;
        let mut item: Installed = serde_json::from_str(legacy).unwrap();
        let dir = test_dir("manifest");
        let emu = crate::emu::Emu { kind: Kind::Eden, dir: dir.clone() };
        std::fs::create_dir_all(dir.join("config")).unwrap();
        assert_eq!(item_paths(&emu, &item).unwrap(), (dir.join("load").join(SMASH_TID).join("FPS"), None));
        item.destination = Destination::Arcropolis;
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""destination":"arcropolis""#));
        let restored: Installed = serde_json::from_str(&json).unwrap();
        assert_eq!(item_paths(&emu, &restored).unwrap().0, dir.join("sdmc/ultimate/mods/FPS"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mixed_flat_archive_routes_normal_trees_separately_from_arc_payload() {
        let dir = test_dir("mixed-flat");
        for file in ["fighter/snake/model.bin", "config.json", "stage/battlefield/model.bin",
            "romfs/normal.bin", "exefs/main.ips", "cheats/codes.txt",
            "ultimate/mods/Explicit/fighter/model.bin", "ultimate/mods/Explicit/romfs/payload.bin",
            "ultimate/mods/Explicit/config.toml"] {
            write_file(&dir, file);
        }
        let roots = roots_from_extracted(&dir, "archive", SMASH_TID).unwrap();
        let normal = roots.iter().find(|r| r.destination == Destination::Emulator).unwrap();
        assert_eq!(normal.files.len(), 3);
        assert!(normal.files.iter().all(|(_, p)| p.starts_with("romfs/") || p.starts_with("exefs/") || p.starts_with("cheats/")));
        let loose = roots.iter().find(|r| r.key == "arcropolis:").unwrap();
        assert_eq!(loose.files.len(), 3);
        assert!(loose.files.iter().any(|(_, p)| p == "config.json"));
        let wrapped = roots.iter().find(|r| r.key == "arcropolis:ultimate/mods/Explicit").unwrap();
        assert_eq!(wrapped.files.len(), 3);
        assert!(wrapped.files.iter().any(|(_, p)| p == "romfs/payload.bin"));
        assert!(wrapped.files.iter().any(|(_, p)| p == "config.toml"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn arcropolis_detection_preserves_root_files_and_scopes_game() {
        let dir = test_dir("loose");
        for file in ["Chief/fighter/snake/model.bin", "Chief/sound/voice.bin", "Chief/ui/icon.bin",
            "Chief/stream;/music.bin", "Chief/config.json", "Chief/custom/other.bin", "readme.txt",
            "FPS/exefs/main.ips"] { write_file(&dir, file); }
        let roots = roots_from_extracted(&dir, "archive", SMASH_TID).unwrap();
        let arc = roots.iter().find(|r| r.destination == Destination::Arcropolis).unwrap();
        assert_eq!(arc.key, "arcropolis:Chief");
        assert_eq!(arc.files.len(), 6);
        assert!(arc.files.iter().any(|(_, p)| p == "config.json"));
        assert!(arc.files.iter().any(|(_, p)| p == "stream;/music.bin"));
        let ordinary = roots.iter().find(|r| r.destination == Destination::Emulator).unwrap();
        assert_eq!(ordinary.files[0].1, "exefs/main.ips");
        let other = roots_from_extracted(&dir, "archive", "0100000000000000").unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].destination, Destination::Emulator);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn arcropolis_wrappers_keep_configs_not_framework_or_wrapper_files() {
        let dir = test_dir("wrapper");
        for file in ["sd/ultimate/mods/A/fighter/x.bin", "sd/ultimate/mods/A/config.toml",
            "sd/ultimate/mods/B/ui/x.bin", "sd/readme.txt",
            "sd/atmosphere/contents/01006A800016E000/romfs/skyline/plugins/libarcropolis.nro"] {
            write_file(&dir, file);
        }
        let roots = roots_from_extracted(&dir, "archive", SMASH_TID).unwrap();
        let arc: Vec<_> = roots.iter().filter(|r| r.destination == Destination::Arcropolis).collect();
        assert_eq!(arc.len(), 2);
        assert_eq!(arc[0].files.len(), 2);
        assert!(arc.iter().flat_map(|r| &r.files).all(|(_, p)| !p.contains("atmosphere") && p != "readme.txt"));
        let loose = test_dir("wrong-game");
        write_file(&loose, "fighter/x.bin");
        assert!(roots_from_extracted(&loose, "archive", "0100000000000000").unwrap().is_empty());
        let explicit = test_dir("wrong-wrapper");
        write_file(&explicit, "ultimate/mods/A/fighter/x.bin");
        assert!(roots_from_extracted(&explicit, "archive", "0100000000000000").err().unwrap().starts_with("ARCropolis requer"));
        let _ = std::fs::remove_dir_all(explicit);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(loose);
    }

    #[test]
    fn stage_and_effect_only_archives_preserve_the_complete_root() {
        let dir = test_dir("stage-effect");
        for file in ["Arena/stage/battlefield/model.bin", "Arena/config.json",
            "Particles/effect/common/effect.bin", "Particles/info.toml"] {
            write_file(&dir, file);
        }
        let roots = roots_from_extracted(&dir, "archive", SMASH_TID).unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|r| r.destination == Destination::Arcropolis && r.files.len() == 2));
        assert!(roots.iter().flat_map(|r| &r.files).any(|(_, p)| p == "config.json"));
        assert!(roots.iter().flat_map(|r| &r.files).any(|(_, p)| p == "info.toml"));
        assert!(roots_from_extracted(&dir, "archive", "0100000000000000").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mario_kart_8_deluxe_raw_files_are_installed_under_romfs() {
        let dir = test_dir("mk8d-raw-romfs");
        for file in [
            "Audio/Driver/Driver_Ludwig.bars",
            "Driver/Ludwig.szs",
            "Kart/BodyTex/BodyB_Std_Ldw_Alb.szs",
            "Course/CourseModel.szs",
            "UI/cmn/tc_Chara_Ludwig.png",
            "readme.txt",
        ] {
            write_file(&dir, file);
        }

        let roots = roots_from_extracted(&dir, "Peter Griffin", "0100152000022000").unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "Peter Griffin");
        assert_eq!(roots[0].destination, Destination::Emulator);
        let mut destinations: Vec<_> = roots[0].files.iter().map(|(_, dest)| dest.as_str()).collect();
        destinations.sort_unstable();
        assert_eq!(destinations, vec![
            "romfs/Audio/Driver/Driver_Ludwig.bars",
            "romfs/Course/CourseModel.szs",
            "romfs/Driver/Ludwig.szs",
            "romfs/Kart/BodyTex/BodyB_Std_Ldw_Alb.szs",
            "romfs/UI/cmn/tc_Chara_Ludwig.png",
        ]);
        assert!(roots_from_extracted(&dir, "Peter Griffin", "0100000000000000").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mk8d_unrecognized_nested_files_are_not_treated_as_romfs() {
        let dir = test_dir("mk8d-unrecognized");
        write_file(&dir, "docs/readme.txt");
        assert!(roots_from_extracted(&dir, "package", "0100152000022000").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn configured_link_root_is_allowed_but_linked_mod_paths_are_rejected() {
        let dir = test_dir("link-boundary");
        let storage = dir.join("storage");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let configured = dir.join("configured");
        let mod_link = storage.join("linked-mod");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&storage, &configured).unwrap();
            std::os::unix::fs::symlink(&outside, &mod_link).unwrap();
        }
        #[cfg(windows)]
        {
            for (link, target) in [(&configured, &storage), (&mod_link, &outside)] {
                assert!(std::process::Command::new("cmd").args(["/C", "mklink", "/J"])
                    .arg(link).arg(target).status().unwrap().success());
            }
        }
        assert_eq!(checked_path(&configured, "safe/fighter/x.bin").unwrap(), configured.join("safe/fighter/x.bin"));
        assert!(checked_path(&configured, "linked-mod/fighter/x.bin").is_err());
        #[cfg(unix)]
        {
            std::fs::remove_file(&configured).unwrap();
            std::fs::remove_file(&mod_link).unwrap();
        }
        #[cfg(windows)]
        {
            std::fs::remove_dir(&configured).unwrap();
            std::fs::remove_dir(&mod_link).unwrap();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn arcropolis_prerequisites_lifecycle_and_boundaries() {
        let dir = test_dir("lifecycle");
        let emu = crate::emu::Emu { kind: Kind::Ryujinx, dir: dir.clone() };
        let mut item = Installed { emu: Kind::Ryujinx, destination: Destination::Arcropolis, tid: SMASH_TID.into(),
            folder: "Chief".into(), mod_id: "local".into(), root_key: "Chief".into(), name: "Chief".into(),
            version: None, installed_at: 0 };
        assert!(check_arcropolis(&emu, SMASH_TID).unwrap_err().starts_with("Skyline/ARCropolis não encontrados"));
        assert!(!emu.sd_dir().join("ultimate").exists());
        for p in ["romfs/skyline/plugins/libarcropolis.nro", "exefs/subsdk9", "exefs/main.npdm"] {
            write_file(&emu.sd_dir().join("atmosphere/contents").join(SMASH_TID), p);
        }
        check_arcropolis(&emu, SMASH_TID).unwrap();
        let (on, off) = item_paths(&emu, &item).unwrap();
        let off = off.unwrap();
        write_file(&on, "fighter/x.bin");
        write_file(&on.parent().unwrap().join("Unmanaged"), "fighter/x.bin");
        toggle_arcropolis(&emu, &item, false).unwrap();
        assert!(!on.exists());
        assert!(off.join("fighter/x.bin").is_file());
        assert!(!item_location(&emu, &item).unwrap().1);
        assert_eq!(unique_folder_pair(on.parent().unwrap(), off.parent().unwrap(), "Chief"), "Chief (2)");
        std::fs::create_dir_all(&on).unwrap();
        assert!(toggle_arcropolis(&emu, &item, true).unwrap_err().starts_with("Pasta de destino já existe:"));
        std::fs::remove_dir(&on).unwrap();
        toggle_arcropolis(&emu, &item, true).unwrap();
        std::fs::remove_dir_all(item_location(&emu, &item).unwrap().0).unwrap();
        assert!(on.parent().unwrap().join("Unmanaged/fighter/x.bin").exists());
        item.folder = "../Unmanaged".into();
        assert!(item_paths(&emu, &item).is_err());
        assert!(checked_path(&dir, "fighter/../../outside").is_err());
        let yuzu = crate::emu::Emu { kind: Kind::Yuzu, dir: dir.clone() };
        assert!(check_arcropolis(&yuzu, SMASH_TID).unwrap_err().starts_with("ARCropolis não é compatível"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unsafe_path_rejects_traversal_and_absolute() {
        for p in ["../x", "a/../../x", "/x", r"romfs\safe\..\..\..\outside.bin"] {
            assert!(unsafe_path(Path::new(p)), "{p}");
        }
        assert!(!unsafe_path(Path::new("romfs/a/b.bin")));
    }

    #[test]
    fn extract_7z_never_writes_outside_target() {
        let base = std::env::temp_dir().join(format!("emm-slip7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zp = base.join("evil.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&zp).unwrap();
        for name in ["../escaped.txt", "ok/romfs/a.bin"] {
            w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(name), Some(&b"x"[..])).unwrap();
        }
        w.finish().unwrap();
        let _ = extract(&zp, &base.join("out"));
        assert!(!base.join("escaped.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn extract_zip_never_writes_outside_target() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!("emm-slip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zp = base.join("evil.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        w.start_file("../escaped.txt", o).unwrap();
        w.write_all(b"x").unwrap();
        w.start_file("ok/romfs/a.bin", o).unwrap();
        w.write_all(b"y").unwrap();
        w.finish().unwrap();
        let out = base.join("out");
        let _ = extract(&zp, &out);
        assert!(!base.join("escaped.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn framework_files_normalizes_tid_case() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!("emm-fw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zp = base.join("fw.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        w.start_file("atmosphere/contents/01006a800016e000/romfs/skyline/plugins/libarcropolis.nro", o).unwrap();
        w.write_all(b"arc").unwrap();
        w.start_file("exefs/subsdk9", o).unwrap();
        w.write_all(b"sky").unwrap();
        w.finish().unwrap();
        let dest = base.join("SD/atmosphere/contents/01006A800016E000");
        framework_files(&zp, &[
            ("skyline/plugins/libarcropolis.nro", "romfs/skyline/plugins/libarcropolis.nro"),
            ("exefs/subsdk9", "exefs/subsdk9"),
        ], &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("romfs/skyline/plugins/libarcropolis.nro")).unwrap(), b"arc");
        assert_eq!(std::fs::read(dest.join("exefs/subsdk9")).unwrap(), b"sky");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn framework_files_rejects_unexpected_layout() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!("emm-fw-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zp = base.join("fw.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
        w.start_file("readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"x").unwrap();
        w.finish().unwrap();
        let dest = base.join("dest");
        let r = framework_files(&zp, &[("exefs/subsdk9", "exefs/subsdk9")], &dest);
        assert_eq!(r, Err("Pacote do Skyline/ARCropolis em formato inesperado".to_string()));
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn arcropolis_resources_copy_missing_and_keep_existing() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!("emm-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zp = base.join("r.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        for (n, b) in [("ultimate/arcropolis/config.json", &b"new"[..]), ("ultimate/arcropolis/resources/a.arc", b"arc"), ("other/x.txt", b"x")] {
            w.start_file(n, o).unwrap();
            w.write_all(b).unwrap();
        }
        w.finish().unwrap();
        let sd = base.join("SD");
        std::fs::create_dir_all(sd.join("ultimate/arcropolis")).unwrap();
        std::fs::write(sd.join("ultimate/arcropolis/config.json"), b"mine").unwrap();
        arcropolis_resources(&zp, &sd).unwrap();
        assert_eq!(std::fs::read(sd.join("ultimate/arcropolis/config.json")).unwrap(), b"mine");
        assert_eq!(std::fs::read(sd.join("ultimate/arcropolis/resources/a.arc")).unwrap(), b"arc");
        assert!(!sd.join("other").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn patch_npdm_sets_game_tid_and_rejects_other_files() {
        let dir = test_dir("npdm");
        let mut b = vec![0u8; 972];
        b[0x330..0x334].copy_from_slice(b"ACI0");
        b[0x340..0x348].copy_from_slice(&0x01006A800016E000u64.to_le_bytes());
        let p = dir.join("main.npdm");
        std::fs::write(&p, &b).unwrap();
        patch_npdm_tid(&p, "0100000000010000").unwrap();
        let out = std::fs::read(&p).unwrap();
        assert_eq!(&out[0x340..0x348], &0x0100000000010000u64.to_le_bytes());
        assert_eq!(&out[..0x340], &b[..0x340]);
        assert_eq!(out.len(), 972);
        std::fs::write(&p, vec![0u8; 972]).unwrap();
        assert_eq!(patch_npdm_tid(&p, "0100000000010000"), Err("Pacote do Skyline/ARCropolis em formato inesperado".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn skyline_zip(path: &Path, npdm: &[u8]) {
        use std::io::Write as _;
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        w.start_file("exefs/subsdk9", o).unwrap();
        w.write_all(b"sky").unwrap();
        w.start_file("exefs/main.npdm", o).unwrap();
        w.write_all(npdm).unwrap();
        w.finish().unwrap();
    }

    fn good_npdm() -> Vec<u8> {
        let mut b = vec![0u8; 972];
        b[0x330..0x334].copy_from_slice(b"ACI0");
        b
    }

    #[test]
    fn skyline_failed_install_publishes_nothing_and_retry_repairs() {
        let base = test_dir("sky-retry");
        let dir = base.join("SD/atmosphere/contents/0100000000010000");
        let zp = base.join("w1/skyline.zip");
        std::fs::create_dir_all(zp.parent().unwrap()).unwrap();
        skyline_zip(&zp, &[0u8; 972]); // NPDM inválido: sem ACI0
        assert!(skyline_from_zip(&zp, &dir, "0100000000010000").is_err());
        assert!(!dir.join(SKYLINE_MARKER).exists() && !dir.join(NPDM_MARKER).exists());
        let zp = base.join("w2/skyline.zip");
        std::fs::create_dir_all(zp.parent().unwrap()).unwrap();
        skyline_zip(&zp, &good_npdm());
        skyline_from_zip(&zp, &dir, "0100000000010000").unwrap();
        assert_eq!(std::fs::read(dir.join(SKYLINE_MARKER)).unwrap(), b"sky");
        assert_eq!(&std::fs::read(dir.join(NPDM_MARKER)).unwrap()[0x340..0x348], &0x0100000000010000u64.to_le_bytes());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn skyline_repairs_only_the_missing_file() {
        let base = test_dir("sky-partial");
        let dir = base.join("SD/atmosphere/contents/0100000000010000");
        let zp = base.join("w/skyline.zip");
        std::fs::create_dir_all(zp.parent().unwrap()).unwrap();
        skyline_zip(&zp, &good_npdm());
        // só o main.npdm sumiu: o subsdk9 do usuário fica intacto
        std::fs::create_dir_all(dir.join("exefs")).unwrap();
        std::fs::write(dir.join(SKYLINE_MARKER), b"mine").unwrap();
        skyline_from_zip(&zp, &dir, "0100000000010000").unwrap();
        assert_eq!(std::fs::read(dir.join(SKYLINE_MARKER)).unwrap(), b"mine");
        assert!(dir.join(NPDM_MARKER).is_file());
        // só o subsdk9 sumiu: o main.npdm do usuário fica intacto
        std::fs::remove_file(dir.join(SKYLINE_MARKER)).unwrap();
        std::fs::write(dir.join(NPDM_MARKER), b"usernpdm").unwrap();
        skyline_from_zip(&zp, &dir, "0100000000010000").unwrap();
        assert_eq!(std::fs::read(dir.join(SKYLINE_MARKER)).unwrap(), b"sky");
        assert_eq!(std::fs::read(dir.join(NPDM_MARKER)).unwrap(), b"usernpdm");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn skyline_plugin_detection() {
        assert!(is_skyline_plugin("romfs/skyline/plugins/libx.nro"));
        assert!(is_skyline_plugin("ROMFS\\Skyline\\Plugins\\X.NRO"));
        assert!(!is_skyline_plugin("romfs/skyline/plugins/config.toml"));
        assert!(!is_skyline_plugin("romfs/data/x.nro"));
        let dir = test_dir("plugins");
        assert!(!has_skyline_plugins(&dir.join("mod")));
        write_file(&dir, "mod/romfs/skyline/plugins/a.nro");
        assert!(has_skyline_plugins(&dir.join("mod")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unique_folder_skips_taken_names() {
        let dir = std::env::temp_dir().join(format!("emm-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(unique_folder(&dir, "m"), "m");
        std::fs::create_dir_all(dir.join("m")).unwrap();
        assert_eq!(unique_folder(&dir, "m"), "m (2)");
        std::fs::create_dir_all(dir.join("m (2)")).unwrap();
        assert_eq!(unique_folder(&dir, "m"), "m (3)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_cases() {
        assert_eq!(sanitize("a:b/c. "), "a_b_c");
        assert_eq!(sanitize("..."), "mod");
    }

    #[test]
    fn encode_cases() {
        assert_eq!(encode_path("A B/[x]+é.txt"), "A%20B/%5Bx%5D%2B%C3%A9.txt");
    }

    #[test]
    #[ignore]
    fn rar_network() {
        let url = format!(
            "https://raw.githubusercontent.com/{}/{}/Mods/0100801011C3E000/1.0.0/60fps.rar",
            catalog::Source::Official.repo(),
            catalog::Source::Official.branch()
        );
        let dir = std::env::temp_dir().join("eden-mod-manager-rar-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = tauri::async_runtime::block_on(async {
            reqwest::Client::new()
                .get(&url)
                .header("User-Agent", UA)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        });
        let rar = dir.join("60fps.rar");
        std::fs::write(&rar, &bytes).unwrap();
        let out = dir.join("x");
        extract(&rar, &out).unwrap();
        let roots = roots_from_extracted(&out, "60fps", "0100801011C3E000").unwrap();
        assert!(roots
            .iter()
            .any(|r| r.files.iter().any(|(_, d)| d.starts_with("exefs/") || d.starts_with("romfs/"))));
    }

    /// GameBanana de ponta a ponta: link (redirect de /dl) → download → extract → detecção de layout.
    /// Tenta os primeiros curados do TotK (até 100 MB cada) até um render roots.
    #[test]
    #[ignore]
    fn gamebanana_install_network() {
        let dir = std::env::temp_dir().join("eden-mod-manager-gb-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let found = tauri::async_runtime::block_on(async {
            let mods = crate::gamebanana::list(None, "0100F2C0115B6000", "The Legend of Zelda: Tears of the Kingdom", false, false)
                .await
                .unwrap();
            for m in mods.mods.iter().take(8) {
                let Ok((url, ext)) = crate::gamebanana::download_url(&m.entry.files[0].src).await else { continue };
                let resp = catalog::HTTP.get(&url).header("User-Agent", UA).send().await.unwrap();
                assert!(resp.status().is_success(), "{url}: {}", resp.status());
                if resp.content_length().is_some_and(|n| n > 100 << 20) {
                    continue;
                }
                let file = dir.join(format!("m.{ext}"));
                std::fs::write(&file, resp.bytes().await.unwrap()).unwrap();
                let out = dir.join("x");
                let _ = std::fs::remove_dir_all(&out);
                if extract(&file, &out).is_ok() && roots_from_extracted(&out, &m.entry.name, "0100F2C0115B6000").is_ok_and(|r| !r.is_empty()) {
                    return true;
                }
            }
            false
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(found, "nenhum dos primeiros curados virou roots");
    }

    /// O arquivo TKMM mais novo não pode substituir o pacote LayeredFS instalável.
    #[test]
    #[ignore]
    fn gamebanana_tkmm_upload_uses_installable_archive() {
        let dir = test_dir("gb-tkmm");
        let result = tauri::async_runtime::block_on(async {
            let (url, ext) = crate::gamebanana::download_url("557157").await?;
            let response = catalog::HTTP
                .get(&url)
                .header("User-Agent", UA)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!("HTTP {} for {url}", response.status()));
            }
            let archive = dir.join(format!("mod.{ext}"));
            let bytes = response.bytes().await.map_err(|e| e.to_string())?;
            std::fs::write(&archive, bytes).map_err(|e| e.to_string())?;
            let out = dir.join("out");
            extract(&archive, &out)?;
            let roots = roots_from_extracted(&out, "Infinite Rocket Shield", "0100F2C0115B6000")?;
            let installable = roots.iter().any(|r| {
                r.destination == Destination::Emulator
                    && r.files.iter().any(|(_, d)| d.starts_with("romfs/") || d.starts_with("exefs/"))
            });
            Ok::<_, String>((ext, installable))
        });
        let _ = std::fs::remove_dir_all(&dir);
        let (ext, installable) = result.unwrap();
        assert_eq!(ext, "7z");
        assert!(installable, "archive did not produce a LayeredFS root");
    }

    /// Extrai a tradução de um jogo pequeno do pacote PT-BR, de ponta a ponta até `roots_from_extracted`.
    #[test]
    #[ignore]
    fn pack_network() {
        let dir = std::env::temp_dir().join("eden-mod-manager-pack-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mods = tauri::async_runtime::block_on(crate::pack::list()).unwrap();
        let m = mods.iter().find(|m| m.tid.as_deref() == Some("01006000040C2000")).unwrap();
        let url = m.source.raw_url(crate::pack::ZIP);
        let mut last = 0;
        crate::pack::extract(&url, &m.files[0].src, m.size, &dir, |r, t| {
            assert_eq!(t, m.size);
            last = r;
        })
        .unwrap();
        assert_eq!(last, m.size);
        let roots = roots_from_extracted(&dir, &m.name, m.tid.as_deref().unwrap()).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, m.name);
        assert!(!roots[0].files.is_empty());
        assert!(roots[0].files.iter().all(|(p, d)| d.starts_with("romfs/") && p.is_file()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_save_files_routes_smash_system_data() {
        let dir = test_dir("save-smash");
        write_file(&dir, "smash_100/save_data/system_data.bin");
        write_file(&dir, "smash_100/save_data/spirits/spirits_param.bin");
        let roots = roots_from_extracted(&dir, "100% Save", SMASH_TID).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].destination, Destination::Save);
        assert_eq!(roots[0].name, "Save: 100% Save");
        let dests: Vec<_> = roots[0].files.iter().map(|(_, d)| d.as_str()).collect();
        assert!(dests.contains(&"save_data/system_data.bin"));
        assert!(dests.contains(&"save_data/spirits/spirits_param.bin"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_save_files_routes_flat_system_data() {
        let dir = test_dir("save-flat");
        write_file(&dir, "system_data.bin");
        let roots = roots_from_extracted(&dir, "Save File", SMASH_TID).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].destination, Destination::Save);
        assert_eq!(roots[0].files[0].1, "save_data/system_data.bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_save_files_routes_other_games_userdata() {
        let dir = test_dir("save-mk8d");
        write_file(&dir, "0100152000022000/userdata.dat");
        let roots = roots_from_extracted(&dir, "MK8D Save", MK8D_TID).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].destination, Destination::Save);
        assert_eq!(roots[0].files[0].1, "userdata.dat");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_dir_all_recursively_copies_all_files() {
        let src = test_dir("copy-src");
        let dst = test_dir("copy-dst");
        write_file(&src, "sub/file1.bin");
        write_file(&src, "file2.bin");
        copy_dir_all(&src, &dst).unwrap();
        assert!(dst.join("sub/file1.bin").is_file());
        assert!(dst.join("file2.bin").is_file());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn emu_save_dir_finds_profile_and_tid() {
        let dir = test_dir("emu-save");
        let emu = crate::emu::Emu { kind: Kind::Eden, dir: dir.clone() };
        let user_save = dir.join("nand/user/save/0000000000000000/1234567890ABCDEF1234567890ABCDEF");
        std::fs::create_dir_all(&user_save).unwrap();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        let save_dir = emu.save_dir(SMASH_TID).unwrap();
        assert_eq!(save_dir, user_save.join(SMASH_TID));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_dir_all_propagates_nonexistent_src_error() {
        let nonexistent = Path::new("nonexistent_path_for_test");
        let dst = test_dir("copy-err");
        assert!(copy_dir_all(nonexistent, &dst).is_err());
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn save_import_rollback_restores_original_save() {
        let dir = test_dir("save-rollback");
        let live_save = dir.join("live_save");
        let backup_dir = dir.join("backup_save");
        write_file(&live_save, "system_data.bin");
        std::fs::write(live_save.join("system_data.bin"), b"ORIGINAL_DATA").unwrap();

        copy_dir_all(&live_save, &backup_dir).unwrap();

        let save_backups = vec![(live_save.clone(), backup_dir.clone())];
        for (live, backup) in save_backups.iter().rev() {
            let _ = std::fs::remove_dir_all(live);
            let _ = copy_dir_all(backup, live);
        }

        assert_eq!(std::fs::read(live_save.join("system_data.bin")).unwrap(), b"ORIGINAL_DATA");
        assert!(backup_dir.join("system_data.bin").is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[derive(Default)]
pub struct PeekCache(Mutex<HashMap<String, Vec<RootInfo>>>);

/// Lista os mods dentro de um pacote sem instalar (baixa em temp, extrai, apaga).
#[tauri::command]
pub async fn peek_archive(
    app: AppHandle,
    cat: State<'_, crate::CatalogState>,
    cache: State<'_, PeekCache>,
    mod_id: String,
) -> Result<Vec<RootInfo>, String> {
    if let Some(r) = cache.0.lock().get(&mod_id) {
        return Ok(r.clone());
    }
    let m = cat
        .0
        .lock()
        .as_ref()
        .and_then(|c| c.mods.iter().find(|m| m.id == mod_id).cloned())
        .ok_or("Mod não encontrado no catálogo")?;
    // contador no nome: leituras em paralelo podem cair no mesmo nanossegundo
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = std::env::temp_dir().join(format!("eden-mod-manager-peek-{nanos:x}-{n}"));
    let _ = std::fs::remove_dir_all(&tmp);
    let r = do_prepare(&app, &m, &tmp, m.tid.as_deref().unwrap_or("")).await;
    let _ = std::fs::remove_dir_all(&tmp);
    let infos: Vec<RootInfo> = r?
        .iter()
        .map(|r| RootInfo { key: r.key.clone(), name: r.name.clone(), file_count: r.files.len(), destination: r.destination })
        .collect();
    cache.0.lock().insert(mod_id, infos.clone());
    Ok(infos)
}
