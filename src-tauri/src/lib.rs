mod addons;
mod catalog;
mod gamebanana;
mod emu;
mod install;
mod nsz;
mod pack;
mod prefs;
mod update;

use catalog::Catalog;
use emu::{Emu, Kind};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

#[derive(Default)]
pub struct CatalogState(pub Mutex<Option<Catalog>>);

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Settings {
    emulator: Kind,
    /// pasta de dados escolhida por emulador
    dirs: HashMap<Kind, String>,
    /// executável escolhido por emulador (quando a detecção automática não acha)
    #[serde(default)]
    exes: HashMap<Kind, String>,
    /// arquivo do jogo escolhido pelo usuário, por TID (maiúsculo)
    #[serde(default)]
    game_files: HashMap<String, String>,
}

#[derive(Clone, Copy)]
pub enum Dir {
    Config,
    Data,
    Cache,
}

/// Pasta do executável quando o modo portátil está ativo (arquivo `portable` ao lado do exe).
pub fn portable_dir() -> Option<PathBuf> {
    let d = std::env::current_exe().ok()?.parent()?.to_path_buf();
    d.join("portable").exists().then_some(d)
}

/// Modo portátil: tudo (configurações, cache, ferramentas) fica em `data/` ao lado do exe,
/// sem tocar no perfil do usuário.
pub fn app_dir(app: &AppHandle, kind: Dir) -> Result<PathBuf, String> {
    if let Some(d) = portable_dir() {
        return Ok(d.join("data").join(match kind {
            Dir::Config => "config",
            Dir::Data => "data",
            Dir::Cache => "cache",
        }));
    }
    let p = app.path();
    match kind {
        Dir::Config => p.app_config_dir(),
        Dir::Data => p.app_data_dir(),
        Dir::Cache => p.app_cache_dir(),
    }
    .map_err(|e| e.to_string())
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_dir(app, Dir::Config)?.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_settings(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let sp = settings_path(app)?;
    std::fs::create_dir_all(sp.parent().unwrap()).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec(s).map_err(|e| e.to_string())?;
    std::fs::write(sp, json).map_err(|e| e.to_string())
}

fn emu_for(s: &Settings, kind: Kind) -> Option<Emu> {
    let dir = s
        .dirs
        .get(&kind)
        .map(PathBuf::from)
        .filter(|p| kind.validate(p))
        .or_else(|| kind.default_dir())?;
    Some(Emu { kind, dir })
}

fn current_emu(app: &AppHandle) -> Option<Emu> {
    let s = load_settings(app);
    emu_for(&s, s.emulator)
}

pub fn resolve_emu(app: &AppHandle) -> Result<Emu, String> {
    current_emu(app).ok_or_else(|| "Pasta do emulador não configurada".to_string())
}

#[derive(Serialize)]
struct EmuInfo {
    kind: Kind,
    dir: Option<String>,
}

#[tauri::command]
fn get_emu(app: AppHandle) -> EmuInfo {
    EmuInfo {
        kind: load_settings(&app).emulator,
        dir: current_emu(&app).map(|e| e.dir.to_string_lossy().into_owned()),
    }
}

#[tauri::command]
fn set_emulator(app: AppHandle, kind: Kind) -> Result<(), String> {
    let mut s = load_settings(&app);
    s.emulator = kind;
    save_settings(&app, &s)
}

#[tauri::command]
fn set_emu_dir(app: AppHandle, kind: Kind, path: String) -> Result<(), String> {
    let mut s = load_settings(&app);
    if !kind.validate(std::path::Path::new(&path)) {
        return Err(kind.invalid_msg().into());
    }
    s.dirs.insert(kind, path);
    save_settings(&app, &s)
}

fn resolved_exe(s: &Settings, emu: &Emu) -> Option<std::path::PathBuf> {
    s.exes.get(&emu.kind).map(std::path::PathBuf::from).filter(|p| p.is_file()).or_else(|| emu::find_exe(emu))
}

/// Executável do emulador ativo (salvo ou detectado); `None` quando o app precisa pedir ao usuário.
#[tauri::command]
fn emu_exe(app: AppHandle) -> Option<String> {
    let s = load_settings(&app);
    resolved_exe(&s, &emu_for(&s, s.emulator)?).map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn set_emu_exe(app: AppHandle, path: String) -> Result<(), String> {
    if !std::path::Path::new(&path).is_file() {
        return Err("Executável do emulador inválido".into());
    }
    let mut s = load_settings(&app);
    s.exes.insert(s.emulator, path);
    save_settings(&app, &s)
}

/// Arquivo do jogo escolhido pelo usuário, para jogos cujo nome não traz o TID (o app não lê o TID de dentro do nsp/xci: precisa de prod.keys).
#[tauri::command]
fn set_game_file(app: AppHandle, tid: String, path: String) -> Result<(), String> {
    emu::check_tid(&tid)?;
    let p = std::path::Path::new(&path);
    if !p.is_file() || !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("nsp") || x.eq_ignore_ascii_case("xci")) {
        return Err("Arquivo do jogo inválido".into());
    }
    let mut s = load_settings(&app);
    s.game_files.insert(tid.to_ascii_uppercase(), path);
    save_settings(&app, &s)
}

/// Abre o jogo no emulador ativo (`-g <arquivo>` no Eden/yuzu; o arquivo como argumento no Ryujinx).
#[tauri::command]
fn launch_game(app: AppHandle, tid: String) -> Result<(), String> {
    emu::check_tid(&tid)?;
    let s = load_settings(&app);
    let emu = emu_for(&s, s.emulator).ok_or("Pasta do emulador não configurada")?;
    let exe = resolved_exe(&s, &emu).ok_or("Executável do emulador não encontrado")?;
    let saved = s.game_files.get(&tid.to_ascii_uppercase()).map(std::path::PathBuf::from).filter(|p| p.is_file());
    let game = match saved { Some(p) => p, None => emu::game_file(&emu, &tid)? };
    let mut cmd = std::process::Command::new(&exe);
    if emu.kind != Kind::Ryujinx { cmd.arg("-g"); }
    // stdio nulo: o app não tem console e herdar handles inválidos faz o CreateProcess falhar (os error 50)
    cmd.arg(&game).current_dir(exe.parent().unwrap_or(std::path::Path::new(".")))
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("Falha ao iniciar o emulador: {e}"))?;
    std::thread::spawn(move || { let _ = child.wait(); }); // colhe o processo; o emulador segue vivo se o app fechar
    Ok(())
}
#[tauri::command]
fn register_game_update(app: AppHandle, tid: String) -> Result<Option<String>, String> {
    emu::check_tid(&tid)?;
    let emu = crate::resolve_emu(&app)?;
    if let Some(up_path) = emu::find_update_file(&emu, &tid) {
        emu::register_update(&emu, &tid, &up_path)?;
        Ok(Some(up_path.to_string_lossy().into_owned()))
    } else {
        Ok(None)
    }
}


#[derive(Serialize)]
struct EmuDir {
    kind: Kind,
    dir: Option<String>,
}

#[tauri::command]
fn get_emu_dirs(app: AppHandle) -> Vec<EmuDir> {
    let s = load_settings(&app);
    [Kind::Eden, Kind::Yuzu, Kind::Ryujinx]
        .map(|kind| EmuDir { kind, dir: emu_for(&s, kind).map(|e| e.dir.to_string_lossy().into_owned()) })
        .into()
}

#[tauri::command]
async fn get_catalog(app: AppHandle, state: State<'_, CatalogState>, force: bool) -> Result<Catalog, String> {
    let cache = app_dir(&app, Dir::Cache)?.join("catalog.json");
    let cached = catalog::load_cached(&cache);
    if !force {
        if let Some(c) = cached
            .as_ref()
            .filter(|c| c.schema == catalog::SCHEMA && catalog::now_secs().saturating_sub(c.fetched_at) < 86400)
        {
            *state.0.lock() = Some(c.clone());
            return Ok(c.clone());
        }
    }
    let cat = match catalog::fetch_catalog(&cache).await {
        Ok(c) => c,
        Err(e) => cached.ok_or(e)?,
    };
    *state.0.lock() = Some(cat.clone());
    Ok(cat)
}

/// Mods do GameBanana para um jogo; entram no catálogo em memória (substituindo os anteriores do jogo)
/// para que `prepare_install` os encontre.
#[tauri::command]
async fn gamebanana_mods(app: AppHandle, state: State<'_, CatalogState>, tid: String, name: String, all: bool, fresh: bool, request_id: u64) -> Result<gamebanana::GbList, String> {
    emu::check_tid(&tid)?;
    // só o pedido mais recente grava no catálogo: um modo antigo e lento não sobrescreve o novo
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let list = gamebanana::list_for_request(Some(&app), &tid, &name, all, fresh, request_id).await?;
    if SEQ.load(std::sync::atomic::Ordering::SeqCst) == n {
        if let Some(c) = state.0.lock().as_mut() {
            c.mods.retain(|m| m.source != catalog::Source::Gamebanana || m.tid.as_deref() != Some(&tid));
            c.mods.extend(list.mods.iter().map(|m| m.entry.clone()));
        }
    }
    Ok(list)
}

#[tauri::command]
async fn gamebanana_detail(id: u64) -> Result<gamebanana::GbDetail, String> {
    gamebanana::detail(id).await
}

#[tauri::command]
async fn gamebanana_top_downloads(name: String) -> Result<Vec<u64>, String> {
    gamebanana::top_downloads(&name).await
}
#[derive(Deserialize)]
struct PrefetchGame {
    tid: String,
    name: Option<String>,
}

#[tauri::command]
fn prefetch_gamebanana(games: Vec<PrefetchGame>) {
    tauri::async_runtime::spawn(async move {
        let pairs: Vec<(String, String)> = games
            .into_iter()
            .filter_map(|g| g.name.map(|n| (g.tid, n)))
            .collect();
        gamebanana::prefetch_curated(pairs).await;
    });
}


#[tauri::command]
fn list_games(app: AppHandle, state: State<'_, CatalogState>) -> Result<Vec<emu::Game>, String> {
    let emu = resolve_emu(&app)?;
    let names = state.0.lock().as_ref().map(|c| c.names.clone()).unwrap_or_default();
    Ok(emu::list_games(&emu, &names))
}

/// Capa de um jogo pela internet (api.nlib.cc), reduzida a 128px e guardada no cache do app.
/// Falhas viram `None`: capa é enfeite, não deve gerar aviso de erro.
#[tauri::command]
async fn game_cover(app: AppHandle, tid: String) -> Option<String> {
    use base64::Engine;
    if tid.len() != 16 || !tid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let file = app_dir(&app, Dir::Cache).ok()?.join("covers").join(format!("{}.jpg", tid.to_lowercase()));
    let bytes = match std::fs::read(&file) {
        Ok(b) => b,
        Err(_) => {
            let resp = reqwest::Client::new()
                .get(format!("https://api.nlib.cc/nx/{tid}/icon"))
                .header("User-Agent", catalog::UA)
                .timeout(std::time::Duration::from_secs(20))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            let raw = resp.bytes().await.ok()?;
            let small = image::load_from_memory(&raw).ok()?.thumbnail(128, 128);
            let mut out = Vec::new();
            small.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).ok()?;
            let _ = std::fs::create_dir_all(file.parent()?);
            let _ = std::fs::write(&file, &out);
            out
        }
    };
    Some(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[tauri::command]
fn open_mod_folder(app: AppHandle, tid: String) -> Result<(), String> {
    emu::check_tid(&tid)?;
    use tauri_plugin_opener::OpenerExt;
    let dir = install::mod_folder(&app, &tid)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(CatalogState::default())
        .manage(install::Pending::default())
        .manage(install::PeekCache::default())
        .setup(|app| {
            if let Some(e) = current_emu(app.handle()) {
                install::cleanup_tmp(&e.mods_dir());
            }
            update::cleanup_old_exe();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_emu,
            set_emulator,
            set_emu_dir,
            get_catalog,
            gamebanana_mods,
            gamebanana_detail,
            gamebanana_top_downloads,
            list_games,
            game_cover,
            open_mod_folder,
            install::prepare_install,
            install::peek_archive,
            install::commit_install,
            install::cancel_install,
            install::prepare_local,
            install::list_installed,
            install::set_mod_enabled,
            install::install_frameworks,
            install::framework_status,
            emu_exe,
            set_emu_exe,
            launch_game,
            set_game_file,
            install::list_conflicts,
            install::uninstall,
            nsz::nsz_run,
            nsz::nsz_can_verify,
            nsz::list_roms,
            update::check_update,
            update::install_update,
            prefs::storage_info,
            prefs::clear_cache,
            prefs::remove_tools,
            get_emu_dirs,
            register_game_update,
            prefetch_gamebanana,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
