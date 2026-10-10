use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use walkdir::WalkDir;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Game {
    pub tid: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub icon: Option<String>,
    pub is_compressed: bool,
    pub update_file: Option<String>,
    pub update_registered: bool,
}

static PV_TID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^01[0-9A-F]{14}$").unwrap());
static VER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+(?:\.\d+)+)").unwrap());

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Eden,
    Yuzu,
    Ryujinx,
}

impl Kind {
    pub fn default_dir(self) -> Option<PathBuf> {
        let (base, name, flatpak) = match self {
            Kind::Eden => (dirs::data_dir()?, "eden", None),
            Kind::Yuzu => (dirs::data_dir()?, "yuzu", Some(("org.yuzu_emu.yuzu", "data"))),
            Kind::Ryujinx => (dirs::config_dir()?, "Ryujinx", Some(("org.ryujinx.Ryujinx", "config"))),
        };
        let mut cands = vec![base.join(name)];
        if let Some(home) = dirs::home_dir() {
            if cfg!(target_os = "linux") {
                if let Some((id, sub)) = flatpak {
                    cands.push(home.join(".var").join("app").join(id).join(sub).join(name));
                }
            }
            // Eden/yuzu: modo portátil guarda tudo em ./user ao lado do executável
            if self != Kind::Ryujinx {
                for d in [home.join(name), home.join("Applications").join(name), home.join("Games").join(name)] {
                    cands.push(d.join("user"));
                }
            }
        }
        // prefere a pasta realmente usada (com config/mods); senão qualquer pasta existente
        cands.iter().find(|p| self.validate(p)).or_else(|| cands.iter().find(|p| p.is_dir())).cloned()
    }

    pub fn validate(self, p: &Path) -> bool {
        match self {
            Kind::Ryujinx => p.join("Config.json").is_file() || p.join("mods").is_dir(),
            _ => qt_config(p).is_file() || p.join("load").is_dir(),
        }
    }

    pub fn invalid_msg(self) -> &'static str {
        match self {
            Kind::Ryujinx => "Pasta inválida (esperado Config.json ou mods/)",
            _ => "Pasta inválida (esperado config/qt-config.ini ou load/)",
        }
    }
}

/// Emulador escolhido + sua pasta de dados.
pub struct Emu {
    pub kind: Kind,
    pub dir: PathBuf,
}

/// No Linux (XDG) config e cache ficam fora da pasta de dados (~/.config/<nome>, ~/.cache/<nome>);
/// no Flatpak, em ~/.var/app/<id>/{config,cache}/<nome>. Windows/macOS: subpasta da própria pasta.
fn xdg_sibling(dir: &Path, sub: &str) -> PathBuf {
    let inner = dir.join(sub);
    if inner.exists() {
        return inner;
    }
    let Some(name) = dir.file_name() else { return inner };
    let flatpak = dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "data"))
        .and_then(|p| p.parent())
        .map(|app| app.join(sub).join(name));
    let xdg = if sub == "config" { dirs::config_dir() } else { dirs::cache_dir() }.map(|d| d.join(name));
    [flatpak, xdg].into_iter().flatten().find(|p| p.as_path() != dir && p.exists()).unwrap_or(inner)
}

pub(crate) fn qt_config(dir: &Path) -> PathBuf {
    xdg_sibling(dir, "config").join("qt-config.ini")
}

fn read_ini(p: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(p) else { return map };
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            map.insert(format!("{section}/{k}"), v.trim().to_string());
        }
    }
    map
}

impl Emu {
    /// Pasta que contém uma subpasta por TID com os mods.
    pub fn mods_dir(&self) -> PathBuf {
        if self.kind == Kind::Ryujinx {
            return self.dir.join("mods").join("contents");
        }
        match read_ini(&qt_config(&self.dir)).get("Data%20Storage/load_directory") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => self.dir.join("load"),
        }
    }

    /// Raiz da SD emulada; no modo portátil `dir` já aponta para `user`.
    pub fn sd_dir(&self) -> PathBuf {
        if self.kind == Kind::Ryujinx {
            return self.dir.join("sdcard");
        }
        let ini = read_ini(&qt_config(&self.dir));
        match ini.get("Data%20Storage/sdmc_directory") {
            Some(v) if !v.trim_matches('"').is_empty()
                && ini.get("Data%20Storage/sdmc_directory\\default").is_none_or(|v| v != "true") =>
            {
                let path = PathBuf::from(v.trim_matches('"'));
                if path.is_absolute() { path } else { self.dir.join(path) }
            }
            _ => self.dir.join("sdmc"),
        }
    }

    /// Ryujinx usa o TID em minúsculas.
    pub fn tid_dir(&self, tid: &str) -> PathBuf {
        let name = if self.kind == Kind::Ryujinx { tid.to_lowercase() } else { tid.to_string() };
        self.mods_dir().join(name)
    }

    pub fn keys_dir(&self) -> PathBuf {
        self.dir.join(if self.kind == Kind::Ryujinx { "system" } else { "keys" })
    }

    pub fn nand_dir(&self) -> PathBuf {
        let ini = read_ini(&qt_config(&self.dir));
        match ini.get("Data%20Storage/nand_directory") {
            Some(v) if !v.trim_matches('"').is_empty()
                && ini.get("Data%20Storage/nand_directory\\default").is_none_or(|v| v != "true") =>
            {
                let path = PathBuf::from(v.trim_matches('"'));
                if path.is_absolute() { path } else { self.dir.join(path) }
            }
            _ => self.dir.join("nand"),
        }
    }

    pub fn save_dir(&self, tid: &str) -> Result<PathBuf, String> {
        check_tid(tid)?;
        match self.kind {
            Kind::Eden | Kind::Yuzu => {
                let nand = self.nand_dir();
                let user_save = nand.join("user/save/0000000000000000");
                if !user_save.is_dir() {
                    return Err("Pasta de saves do emulador não encontrada. Inicie o jogo ao menos uma vez.".into());
                }
                let entries: Vec<_> = std::fs::read_dir(&user_save)
                    .map_err(|e| format!("Falha ao ler pasta de saves: {e}"))?
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
                    .map(|e| e.path())
                    .collect();
                if entries.is_empty() {
                    return Err("Nenhum perfil de usuário encontrado no emulador. Inicie o jogo ao menos uma vez.".into());
                }
                let tid_upper = tid.to_ascii_uppercase();
                let matching: Vec<_> = entries.iter().filter(|p| p.join(&tid_upper).is_dir()).collect();
                if matching.len() == 1 {
                    Ok(matching[0].join(&tid_upper))
                } else if matching.len() > 1 {
                    Err("Múltiplos perfis de usuário encontrados com save deste jogo. Destino ambíguo.".into())
                } else if entries.len() == 1 {
                    Ok(entries[0].join(&tid_upper))
                } else {
                    Err("Pasta de save deste jogo não encontrada nos perfis. Inicie o jogo ao menos uma vez no emulador.".into())
                }
            }
            Kind::Ryujinx => {
                let user_save = self.dir.join("bis/user/save");
                if !user_save.is_dir() {
                    return Err("Pasta de saves do emulador não encontrada. Inicie o jogo ao menos uma vez.".into());
                }
                let info_file = user_save.join("ExtraSaveDirInfo");
                if let Ok(info) = std::fs::read_to_string(&info_file) {
                    for line in info.lines() {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 2 && parts[0].eq_ignore_ascii_case(tid) {
                            return Ok(user_save.join(parts[1]).join("0"));
                        }
                    }
                }
                Err("Save deste jogo não encontrado no Ryujinx. Inicie o jogo ao menos uma vez no emulador.".into())
            }
        }
    }

    pub fn is_running(&self) -> bool {
        let exe_names = match self.kind {
            Kind::Eden => &["eden.exe", "eden"][..],
            Kind::Yuzu => &["yuzu.exe", "yuzu"][..],
            Kind::Ryujinx => &["ryujinx.exe", "Ryujinx.exe", "Ryujinx", "ryujinx"][..],
        };
        #[cfg(windows)]
        {
            let output = std::process::Command::new("tasklist")
                .args(["/NH", "/FO", "CSV"])
                .output();
            if let Ok(out) = output {
                let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
                for name in exe_names {
                    if stdout.contains(&name.to_lowercase()) {
                        return true;
                    }
                }
            }
            false
        }
        #[cfg(not(windows))]
        {
            for name in exe_names {
                let output = std::process::Command::new("pgrep")
                    .arg("-x")
                    .arg(name)
                    .output();
                if output.is_ok_and(|o| !o.stdout.is_empty()) {
                    return true;
                }
            }
            false
        }
    }
}

/// O TID vem do webview e vira nome de pasta: só 16 dígitos hexadecimais (nada de `..`, `\` ou caminho absoluto).
pub fn check_tid(tid: &str) -> Result<(), String> {
    if tid.len() == 16 && tid.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("TID inválido".into())
    }
}

/// Apenas IDs que terminam em 000 são jogos base no Switch (updates terminam em 800 e DLCs em outros sufixos).
pub fn is_base_tid(tid: &str) -> bool {
    tid.len() == 16 && tid.ends_with("000") && PV_TID.is_match(tid)
}

/// TID de update esperado para um jogo base (+0x800).
pub fn update_tid(base_tid: &str) -> Option<String> {
    u64::from_str_radix(base_tid, 16).ok().map(|v| format!("{:016X}", v + 0x800))
}


#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RomFile {
    pub path: String,
    pub name: String,
    pub ext: String,
    pub size: u64,
}

/// Pastas de jogos configuradas no emulador: (caminho, varredura recursiva).
fn configured_game_dirs(emu: &Emu) -> Vec<(String, bool)> {
    if emu.kind == Kind::Ryujinx {
        let cfg: serde_json::Value = std::fs::read(emu.dir.join("Config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // ponytail: assume varredura recursiva; o Config.json não tem esse flag
        return cfg["game_dirs"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| (s.to_string(), true)).collect())
            .unwrap_or_default();
    }
    let ini = read_ini(&qt_config(&emu.dir));
    let n: usize = ini.get("UI/Paths\\gamedirs\\size").and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut out = Vec::new();
    for i in 1..=n {
        let Some(path) = ini.get(&format!("UI/Paths\\gamedirs\\{i}\\path")) else { continue };
        if matches!(path.as_str(), "SDMC" | "UserNAND" | "SysNAND") {
            continue;
        }
        let deep = ini.get(&format!("UI/Paths\\gamedirs\\{i}\\deep_scan")).is_some_and(|v| v == "true");
        out.push((path.clone(), deep));
    }
    out
}

/// Pastas configuradas; se não houver nenhuma existente, procura pastas comuns de jogos.
fn game_dirs(emu: &Emu) -> Vec<(String, bool)> {
    let mut out = configured_game_dirs(emu);
    if out.iter().any(|(p, _)| Path::new(p).is_dir()) {
        return out;
    }
    let Some(home) = dirs::home_dir() else { return out };
    let docs = dirs::document_dir().unwrap_or_else(|| home.join("Documents"));
    let bases = [home.clone(), docs];
    for b in &bases {
        for n in ["Games", "Jogos", "Roms", "ROMs", "Switch", "Nintendo Switch"] {
            for sub in ["", "Switch", "switch", "Nintendo Switch"] {
                let p = if sub.is_empty() { b.join(n) } else { b.join(n).join(sub) };
                let s = p.to_string_lossy().into_owned();
                if p.is_dir() && !out.iter().any(|(o, _)| *o == s) {
                    out.push((s, true));
                }
            }
        }
    }
    out
}

/// Arquivos nsp|xci|nsz|xcz nas pastas de jogos do emulador.
pub fn rom_files(emu: &Emu) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (path, deep) in game_dirs(emu) {
        if !Path::new(&path).is_dir() {
            continue;
        }
        for e in WalkDir::new(&path).max_depth(if deep { usize::MAX } else { 1 }).into_iter().flatten() {
            let ext = e.path().extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if matches!(ext.as_str(), "nsp" | "xci" | "nsz" | "xcz") {
                out.push(e.path().to_path_buf());
            }
        }
    }
    out
}

impl Kind {
    fn exe_names(self) -> &'static [&'static str] {
        match self {
            Kind::Eden => &["eden.exe", "eden"],
            Kind::Yuzu => &["yuzu.exe", "yuzu"],
            Kind::Ryujinx => &["Ryujinx.exe", "Ryujinx", "ryujinx"],
        }
    }
}

/// Procura o executável do emulador: pasta do modo portátil, pastas comuns de instalação/extração e o PATH.
/// `None` quando não acha; o app então pede o arquivo uma vez e o guarda nas configurações.
pub fn find_exe(emu: &Emu) -> Option<PathBuf> {
    let names = emu.kind.exe_names();
    let mut dirs: Vec<PathBuf> = Vec::new();
    // portátil: `user` (Eden/yuzu) ou `portable` (Ryujinx) ficam ao lado do executável
    if emu.dir.file_name().is_some_and(|n| n == "user" || n == "portable") {
        dirs.extend(emu.dir.parent().map(Path::to_path_buf));
    }
    let folder = match emu.kind { Kind::Eden => "eden", Kind::Yuzu => "yuzu", Kind::Ryujinx => "Ryujinx" };
    let home = dirs::home_dir();
    let bases = [dirs::data_local_dir(), home.clone(), home.as_ref().map(|h| h.join("Games")), dirs::document_dir(),
        Some(PathBuf::from("C:\\Program Files")), Some(PathBuf::from("/Applications")), Some(PathBuf::from("/opt"))];
    for b in bases.into_iter().flatten() {
        for name in [folder, &folder.to_lowercase()] {
            let d = b.join(name);
            // a própria pasta e um nível abaixo (ex.: `yuzu/yuzu-windows-msvc`, `Eden-Windows-x64`)
            let subs = std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir());
            dirs.push(d.clone());
            dirs.extend(subs);
        }
    }
    dirs.extend(std::env::var_os("PATH").iter().flat_map(std::env::split_paths));
    dirs.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
}

/// Arquivo iniciável do jogo `tid` (nsp/xci do jogo base; updates e DLC não iniciam sozinhos).
pub fn game_file(emu: &Emu, tid: &str) -> Result<PathBuf, String> {
    let found: Vec<PathBuf> = rom_files(emu).into_iter().filter(|p| {
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        crate::catalog::last_tid(&stem).is_some_and(|t| t.eq_ignore_ascii_case(tid))
    }).collect();
    if found.is_empty() { return Err("Arquivo do jogo não encontrado nas pastas de jogos do emulador".into()); }
    found.into_iter()
        .find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("nsp") || x.eq_ignore_ascii_case("xci")))
        .ok_or_else(|| "Jogo compactado (nsz/xcz): descomprima na aba NSZ para iniciar".into())
}

/// Procura o arquivo de update mais recente para o jogo base (+0x800).
pub fn find_update_file(emu: &Emu, base_tid: &str) -> Option<PathBuf> {
    let up_tid = update_tid(base_tid)?;
    let mut ups: Vec<PathBuf> = rom_files(emu)
        .into_iter()
        .filter(|p| {
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            crate::catalog::last_tid(&stem).is_some_and(|t| t.eq_ignore_ascii_case(&up_tid))
        })
        .collect();
    ups.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    ups.into_iter().next()
}

pub(crate) fn add_external_content_dir(text: &str, dir: &str) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut size_idx = None;
    let mut current_size = 0usize;
    let mut ui_idx = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "[UI]" {
            ui_idx = Some(i);
        }
        if trimmed.starts_with("Paths\\external_content_dirs\\size=") {
            size_idx = Some(i);
            if let Some(s) = trimmed.strip_prefix("Paths\\external_content_dirs\\size=") {
                current_size = s.parse().unwrap_or(0);
            }
        }
    }

    let new_idx = current_size + 1;
    let new_entry = format!("Paths\\external_content_dirs\\{new_idx}\\path={dir}");

    if let Some(idx) = size_idx {
        lines[idx] = format!("Paths\\external_content_dirs\\size={new_idx}");
        lines.insert(idx + 1, new_entry);
    } else if let Some(idx) = ui_idx {
        lines.insert(idx + 1, "Paths\\external_content_dirs\\size=1".into());
        lines.insert(idx + 2, format!("Paths\\external_content_dirs\\1\\path={dir}"));
    } else {
        lines.push("[UI]".into());
        lines.push("Paths\\external_content_dirs\\size=1".into());
        lines.push(format!("Paths\\external_content_dirs\\1\\path={dir}"));
    }

    lines.join(eol) + eol
}

/// Registra o update no emulador (Ryujinx: updates.json; Eden/yuzu: Paths\\external_content_dirs).
pub fn register_update(emu: &Emu, base_tid: &str, update_path: &Path) -> Result<(), String> {
    if emu.kind == Kind::Ryujinx {
        let dir = emu.dir.join("games").join(base_tid.to_lowercase());
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Falha ao criar pasta de configuração do Ryujinx: {e}"))?;
        let path_str = update_path.to_string_lossy().to_string();
        let file = dir.join("updates.json");
        let mut data: serde_json::Value = if file.exists() {
            let bytes = std::fs::read(&file)
                .map_err(|e| format!("Falha ao ler updates.json do Ryujinx: {e}"))?;
            serde_json::from_slice(&bytes)
                .map_err(|e| format!("updates.json corrompido: {e}"))?
        } else {
            serde_json::json!({ "selected": null, "paths": [] })
        };
        let mut paths: Vec<String> = data["paths"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if !paths.iter().any(|p| p.eq_ignore_ascii_case(&path_str)) {
            paths.push(path_str.clone());
        }
        data["selected"] = serde_json::Value::String(path_str);
        data["paths"] = serde_json::Value::Array(paths.into_iter().map(serde_json::Value::String).collect());
        let bytes = serde_json::to_vec_pretty(&data)
            .map_err(|e| format!("Falha ao serializar updates.json: {e}"))?;
        std::fs::write(&file, bytes)
            .map_err(|e| format!("Falha ao gravar updates.json do Ryujinx: {e}"))?;
    } else {
        let cfg_path = qt_config(&emu.dir);
        let text = std::fs::read_to_string(&cfg_path)
            .map_err(|e| format!("Falha ao ler configuração do emulador: {e}"))?;
        let update_dir = update_path.parent().unwrap_or(update_path);
        let dir_str = update_dir.to_string_lossy().replace('\\', "/");
        let ini = read_ini(&cfg_path);
        let n: usize = ini.get("UI/Paths\\external_content_dirs\\size").and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut exists = false;
        let norm_u = dir_str.trim_end_matches('/').to_lowercase();
        for i in 1..=n {
            if let Some(p) = ini.get(&format!("UI/Paths\\external_content_dirs\\{i}\\path")) {
                let norm_p = p.replace('\\', "/").trim_end_matches('/').to_lowercase();
                if norm_p == norm_u || norm_u.starts_with(&norm_p) {
                    exists = true;
                    break;
                }
            }
        }
        if !exists {
            let new_text = add_external_content_dir(&text, &dir_str);
            std::fs::write(&cfg_path, new_text)
                .map_err(|e| format!("Falha ao gravar configuração do emulador: {e}"))?;
        }
    }
    Ok(())
}

pub fn list_games(emu: &Emu, names: &HashMap<String, String>) -> Vec<Game> {
    let mut games: HashMap<String, Game> = HashMap::new();

    // (a) cache da lista de jogos (Eden/yuzu): apenas jogos base (TID terminando em 000)
    if let Ok(rd) = std::fs::read_dir(xdg_sibling(&emu.dir, "cache").join("game_list")) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let (stem, is_icon) = match (fname.strip_suffix(".pv.txt"), fname.strip_suffix(".jpeg")) {
                (Some(s), _) => (s, false),
                (_, Some(s)) => (s, true),
                _ => continue,
            };
            let tid = stem.to_uppercase();
            if !is_base_tid(&tid) {
                continue;
            }
            let g = games.entry(tid.clone()).or_insert_with(|| Game {
                tid: tid.clone(),
                name: None,
                version: None,
                icon: None,
                is_compressed: false,
                update_file: None,
                update_registered: false,
            });
            if is_icon {
                // capa do jogo base (o ícone de update/DLC é ignorado)
                if stem.eq_ignore_ascii_case(&tid) {
                    g.icon = std::fs::read(e.path()).ok().map(|b| {
                        format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(b))
                    });
                }
                continue;
            }
            let version = std::fs::read_to_string(e.path())
                .ok()
                .and_then(|c| VER.captures(&c).map(|m| m[1].to_string()));
            if g.version.is_none() {
                g.version = version;
            }
        }
    }

    // (a') Ryujinx: games/<tid em minúsculas>/ existe para cada jogo já aberto
    if emu.kind == Kind::Ryujinx {
        if let Ok(rd) = std::fs::read_dir(emu.dir.join("games")) {
            for e in rd.flatten() {
                let tid = e.file_name().to_string_lossy().to_uppercase();
                if is_base_tid(&tid) {
                    games.entry(tid.clone()).or_insert_with(|| Game {
                        tid,
                        name: None,
                        version: None,
                        icon: None,
                        is_compressed: false,
                        update_file: None,
                        update_registered: false,
                    });
                }
            }
        }
    }

    let all_roms = rom_files(emu);

    // (b) varredura das pastas de jogos: apenas arquivos do jogo base
    for p in &all_roms {
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let Some(tid) = crate::catalog::last_tid(&stem) else { continue };
        let tid_upper = tid.to_uppercase();
        if !is_base_tid(&tid_upper) {
            continue;
        }
        let name = stem.split(" [").next().unwrap_or(&stem).trim().to_string();
        let g = games
            .entry(tid_upper.clone())
            .or_insert_with(|| Game {
                tid: tid_upper,
                name: None,
                version: None,
                icon: None,
                is_compressed: false,
                update_file: None,
                update_registered: false,
            });
        if g.name.is_none() && !name.is_empty() {
            g.name = Some(name);
        }
    }

    // Verifica compressão e detecta updates de cada jogo
    for g in games.values_mut() {
        let base_roms: Vec<&PathBuf> = all_roms
            .iter()
            .filter(|p| {
                let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                crate::catalog::last_tid(&stem).is_some_and(|t| t.eq_ignore_ascii_case(&g.tid))
            })
            .collect();
        let has_uncompressed = base_roms.iter().any(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("nsp") || x.eq_ignore_ascii_case("xci")));
        let has_compressed = base_roms.iter().any(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("nsz") || x.eq_ignore_ascii_case("xcz")));
        g.is_compressed = has_compressed && !has_uncompressed;

        if let Some(up_path) = find_update_file(emu, &g.tid) {
            g.update_file = Some(up_path.to_string_lossy().into_owned());
            if g.version.is_none() {
                let stem = up_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                g.version = VER.captures(&stem).map(|m| m[1].to_string()).or_else(|| {
                    regex::Regex::new(r"\[v(\d+)\]").ok().and_then(|re| re.captures(&stem).map(|m| format!("v{}", &m[1])))
                });
            }
            g.update_registered = register_update(emu, &g.tid, &up_path).is_ok();
        }
    }

    let mut out: Vec<Game> = games.into_values().collect();
    for g in &mut out {
        if g.name.is_none() {
            g.name = names.get(&g.tid).cloned();
        }
    }
    out.sort_by(|a, b| {
        (a.name.as_deref().unwrap_or("~").to_lowercase(), &a.tid)
            .cmp(&(b.name.as_deref().unwrap_or("~").to_lowercase(), &b.tid))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sd_directory_uses_portable_root_or_custom_qt_storage() {
        let dir = std::env::temp_dir().join(format!("emm-sd-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("user/config")).unwrap();
        let emu = Emu { kind: Kind::Eden, dir: dir.join("user") };
        assert_eq!(emu.sd_dir(), emu.dir.join("sdmc"));
        let custom = dir.join("Custom SD");
        let config = emu.dir.join("config/qt-config.ini");
        std::fs::write(&config, format!("[Data%20Storage]\nsdmc_directory=\"{}\"\nsdmc_directory\\default=false\n", custom.display())).unwrap();
        assert_eq!(emu.sd_dir(), custom);
        std::fs::write(&config, "[Data%20Storage]\nsdmc_directory=old-path\nsdmc_directory\\default=true\n").unwrap();
        assert_eq!(emu.sd_dir(), emu.dir.join("sdmc"));
        std::fs::write(&config, "[Data%20Storage]\nsdmc_directory=custom-sd\nsdmc_directory\\default=false\n").unwrap();
        assert_eq!(emu.sd_dir(), emu.dir.join("custom-sd"));
        let ryu = Emu { kind: Kind::Ryujinx, dir: dir.clone() };
        assert_eq!(ryu.sd_dir(), dir.join("sdcard"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ryujinx_layout() {
        let dir = std::env::temp_dir().join("eden-mod-manager-ryu-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Config.json"), r#"{"game_dirs":["D:/Jogos","E:/x"]}"#).unwrap();
        assert!(Kind::Ryujinx.validate(&dir));
        assert!(!Kind::Eden.validate(&dir));
        let emu = Emu { kind: Kind::Ryujinx, dir: dir.clone() };
        assert_eq!(game_dirs(&emu), vec![("D:/Jogos".into(), true), ("E:/x".into(), true)]);
        assert_eq!(emu.tid_dir("0100ABCD00001000"), dir.join("mods").join("contents").join("0100abcd00001000"));
        assert_eq!(emu.keys_dir(), dir.join("system"));
    }

    #[test]
    fn game_file_prefers_base_game_and_rejects_compressed_only() {
        let root = std::env::temp_dir().join(format!("emm-game-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let games = root.join("games");
        std::fs::create_dir_all(&games).unwrap();
        std::fs::create_dir_all(root.join("user/config")).unwrap();
        let ini = format!("[UI]\nPaths\\gamedirs\\size=1\nPaths\\gamedirs\\1\\path={}\nPaths\\gamedirs\\1\\deep_scan=true\n", games.display());
        std::fs::write(root.join("user/config/qt-config.ini"), ini).unwrap();
        let emu = Emu { kind: Kind::Eden, dir: root.join("user") };
        let tid = "0100F2C0115B6000";
        assert!(game_file(&emu, tid).unwrap_err().starts_with("Arquivo do jogo não encontrado"));
        // update (+0x800) não inicia sozinho; o jogo base compactado (nsz) é recusado com dica
        std::fs::write(games.join("Zelda [0100F2C0115B6800][v65536].nsp"), b"u").unwrap();
        assert!(game_file(&emu, tid).unwrap_err().starts_with("Arquivo do jogo não encontrado"));
        std::fs::write(games.join("Zelda [0100F2C0115B6000][v0].nsz"), b"z").unwrap();
        assert!(game_file(&emu, tid).unwrap_err().starts_with("Jogo compactado"));
        std::fs::write(games.join("Zelda [0100F2C0115B6000][v0].xci"), b"x").unwrap();
        assert_eq!(game_file(&emu, tid).unwrap(), games.join("Zelda [0100F2C0115B6000][v0].xci"));
        std::fs::remove_file(games.join("Zelda [0100F2C0115B6000][v0].xci")).unwrap();
        assert!(game_file(&emu, tid).unwrap_err().starts_with("Jogo compactado"));
        // modo portátil: o executável fica ao lado da pasta `user`
        std::fs::write(root.join(Kind::Eden.exe_names()[0]), b"").unwrap();
        assert_eq!(find_exe(&emu), Some(root.join(Kind::Eden.exe_names()[0])));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn flatpak_layout_finds_config_outside_data_dir() {
        let root = std::env::temp_dir().join("eden-mod-manager-flatpak-test");
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data").join("yuzu");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(root.join("config").join("yuzu")).unwrap();
        std::fs::write(root.join("config").join("yuzu").join("qt-config.ini"), "[Data%20Storage]\nload_directory=/x/load\n").unwrap();
        assert!(Kind::Yuzu.validate(&data));
        let emu = Emu { kind: Kind::Yuzu, dir: data };
        assert_eq!(emu.mods_dir(), PathBuf::from("/x/load"));
    }

    #[test]
    fn base_tid_detection_and_update_calculation() {
        assert!(is_base_tid("01007EF00011E000"));
        assert!(is_base_tid("0100152000022000"));
        // Update (+0x800) e DLC (+0x1001) não são jogos base
        assert!(!is_base_tid("01007EF00011E800"));
        assert!(!is_base_tid("0100152000022800"));
        assert!(!is_base_tid("0100152000023001"));
        assert_eq!(update_tid("01007EF00011E000"), Some("01007EF00011E800".into()));
        assert_eq!(update_tid("0100152000022000"), Some("0100152000022800".into()));
    }

    #[test]
    fn external_content_dirs_manipulation() {
        let ini = "[UI]\nPaths\\gamedirs\\size=1\n";
        let out = add_external_content_dir(ini, "D:/Jogos/Updates");
        assert!(out.contains("Paths\\external_content_dirs\\size=1"));
        assert!(out.contains("Paths\\external_content_dirs\\1\\path=D:/Jogos/Updates"));
        let out2 = add_external_content_dir(&out, "E:/Outro");
        assert!(out2.contains("Paths\\external_content_dirs\\size=2"));
        assert!(out2.contains("Paths\\external_content_dirs\\2\\path=E:/Outro"));
    }

    #[test]
    fn ryujinx_register_update() {
        let dir = std::env::temp_dir().join(format!("emm-ryu-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let emu = Emu { kind: Kind::Ryujinx, dir: dir.clone() };
        let tid = "01007EF00011E000";
        let update_path = PathBuf::from("D:/Jogos/Zelda_Update.nsp");
        register_update(&emu, tid, &update_path).unwrap();
        let json_path = dir.join("games").join(tid.to_lowercase()).join("updates.json");
        assert!(json_path.is_file());
        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(json_path).unwrap()).unwrap();
        assert_eq!(val["selected"], "D:/Jogos/Zelda_Update.nsp");
        assert_eq!(val["paths"], serde_json::json!(["D:/Jogos/Zelda_Update.nsp"]));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn list_games_filters_updates_dlcs_and_detects_compression() {
        let root = std::env::temp_dir().join(format!("emm-list-games-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let games_dir = root.join("games");
        std::fs::create_dir_all(&games_dir).unwrap();
        std::fs::create_dir_all(root.join("user/config")).unwrap();
        let ini = format!("[UI]\nPaths\\gamedirs\\size=1\nPaths\\gamedirs\\1\\path={}\nPaths\\gamedirs\\1\\deep_scan=true\n", games_dir.display());
        std::fs::write(root.join("user/config/qt-config.ini"), ini).unwrap();
        let emu = Emu { kind: Kind::Eden, dir: root.join("user") };

        // 1. Jogo base em NSZ (compactado) + update em NSP
        std::fs::write(games_dir.join("Zelda [01007EF00011E000][v0].nsz"), b"z").unwrap();
        std::fs::write(games_dir.join("Zelda [01007EF00011E800][v1114112].nsp"), b"u").unwrap();

        // 2. Arquivos avulsos de update e DLC de outro jogo sem o jogo base
        std::fs::write(games_dir.join("Standalone [0100AAAA00001800][v65536].nsp"), b"u").unwrap();
        std::fs::write(games_dir.join("DLC Only [0100AAAA00002001][v0].nsp"), b"d").unwrap();

        let games = list_games(&emu, &HashMap::new());
        // Deve listar APENAS o jogo base Zelda (1 jogo)
        assert_eq!(games.len(), 1);
        let g = &games[0];
        assert_eq!(g.tid, "01007EF00011E000");
        assert!(g.is_compressed);
        assert!(g.update_file.is_some());
        assert_eq!(g.version, Some("v1114112".into()));
        assert!(g.update_registered);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn register_update_propagates_missing_or_unwriteable_config_failures() {
        let dir = std::env::temp_dir().join(format!("emm-reg-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let emu = Emu { kind: Kind::Eden, dir: dir.clone() };
        let tid = "01007EF00011E000";
        let update_path = PathBuf::from("D:/Jogos/Update.nsp");
        // Falha: pasta do emulador/config não existe
        assert!(register_update(&emu, tid, &update_path).is_err());

        // Ryujinx: updates.json corrompido
        let ryu_dir = dir.join("ryu");
        std::fs::create_dir_all(ryu_dir.join("games").join(tid.to_lowercase())).unwrap();
        std::fs::write(ryu_dir.join("games").join(tid.to_lowercase()).join("updates.json"), b"corrupted{").unwrap();
        let ryu = Emu { kind: Kind::Ryujinx, dir: ryu_dir };
        assert!(register_update(&ryu, tid, &update_path).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_dir_rejects_ambiguity_and_missing_mappings() {
        let dir = std::env::temp_dir().join(format!("emm-savedir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let emu = Emu { kind: Kind::Eden, dir: dir.clone() };
        let tid = "01006A800016E000";

        // Sem pasta user/save
        assert!(emu.save_dir(tid).is_err());

        // Múltiplos perfis sem save do jogo -> rejeita como ambíguo
        let save_root = dir.join("nand/user/save/0000000000000000");
        std::fs::create_dir_all(save_root.join("PROFILE_A")).unwrap();
        std::fs::create_dir_all(save_root.join("PROFILE_B")).unwrap();
        let err = emu.save_dir(tid).unwrap_err();
        assert!(err.contains("não encontrada nos perfis"));

        // Múltiplos perfis com save do jogo -> rejeita como ambíguo
        std::fs::create_dir_all(save_root.join("PROFILE_A").join(tid)).unwrap();
        std::fs::create_dir_all(save_root.join("PROFILE_B").join(tid)).unwrap();
        let err2 = emu.save_dir(tid).unwrap_err();
        assert!(err2.contains("Destino ambíguo"));

        // Exatamente um perfil com save do jogo -> aceita sem ambiguidade
        std::fs::remove_dir_all(save_root.join("PROFILE_B")).unwrap();
        assert_eq!(emu.save_dir(tid).unwrap(), save_root.join("PROFILE_A").join(tid));

        // Ryujinx: sem mapeamento no ExtraSaveDirInfo -> rejeita sem adivinhar pastas alheias
        let ryu_dir = dir.join("ryu");
        std::fs::create_dir_all(ryu_dir.join("bis/user/save/0000000000000001/0")).unwrap();
        let ryu = Emu { kind: Kind::Ryujinx, dir: ryu_dir.clone() };
        assert!(ryu.save_dir(tid).is_err());

        // Ryujinx: com mapeamento no ExtraSaveDirInfo -> aceita pasta correta
        std::fs::write(ryu_dir.join("bis/user/save/ExtraSaveDirInfo"), format!("{tid} 0000000000000001\n")).unwrap();
        assert_eq!(ryu.save_dir(tid).unwrap(), ryu_dir.join("bis/user/save/0000000000000001/0"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
