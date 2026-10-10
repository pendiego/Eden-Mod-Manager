//! GameBanana (API pública v11). Diferente das outras fontes, não há árvore para indexar: os mods
//! são buscados por jogo, na hora em que o usuário abre o jogo, e entram no catálogo em memória.
//! Modo curado = destacados pelo site (`Featured`) ou com `MIN_LIKES` curtidas ou mais.
use crate::catalog::{self, ModEntry, ModFile, ModKind, Source, HTTP_JSON, UA};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const API: &str = "https://gamebanana.com/apiv11";
const MIN_LIKES: u32 = 50;
const PER_PAGE: u32 = 50;
// teto de segurança contra paginação infinita (5000 mods por consulta); o fim normal é `_bIsComplete`
const MAX_PAGES: u32 = 100;
// páginas pedidas em paralelo; o índice pesa ~380 KB por página sem gzip
const BATCH: u32 = 6;
const MAX_INFLIGHT: usize = 6;
const PREFETCH_GAMES: usize = MAX_INFLIGHT / 2;
const TTL: Duration = Duration::from_secs(600);

// pedidos simultâneos ao GameBanana (todas as varreduras juntas)
static INFLIGHT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_INFLIGHT);
#[derive(Clone)]
struct CacheEntry {
    at: Instant,
    mods: Vec<GbMod>,
    complete: bool,
}

// (tid, todos?) -> CacheEntry; vale `TTL`
static CACHE: LazyLock<Mutex<HashMap<(String, bool), CacheEntry>>> = LazyLock::new(Default::default);
// cache de id do jogo no GameBanana por nome normalizado (evita roundtrip ao Util/Search/Results)
static GAME_IDS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);

#[derive(Deserialize)]
struct Page<T> {
    #[serde(rename = "_aMetadata")]
    meta: Meta,
    #[serde(rename = "_aRecords")]
    records: Vec<T>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "_bIsComplete")]
    complete: bool,
}

#[derive(Deserialize, Clone)]
struct Rec {
    #[serde(rename = "_idRow")]
    id: u64,
    #[serde(rename = "_sName")]
    name: String,
    #[serde(rename = "_bHasFiles", default)]
    has_files: bool,
    #[serde(rename = "_bIsObsolete", default)]
    obsolete: bool,
    #[serde(rename = "_nLikeCount", default)]
    likes: u32,
    #[serde(rename = "_bWasFeatured", default)]
    featured: bool,
    #[serde(rename = "_nViewCount", default)]
    views: u32,
    #[serde(rename = "_bHasContentRatings", default)]
    has_content_ratings: bool,
    #[serde(rename = "_aRootCategory", default)]
    root_category: serde_json::Value,
    // Value: APIs PHP mandam `[]` no lugar de objeto vazio
    #[serde(rename = "_aPreviewMedia", default)]
    media: serde_json::Value,
}

#[derive(Deserialize)]
struct GameRec {
    #[serde(rename = "_idRow")]
    id: u64,
    #[serde(rename = "_sName")]
    name: String,
}

const MAX_RETRIES: u32 = 3;

/// Pausa antes da nova tentativa após um 429: `Retry-After` se o site informar, senão 2 s, 4 s, 8 s; no máximo 15 s.
fn backoff(tries: u32, retry_after: Option<u64>) -> Duration {
    Duration::from_secs(retry_after.unwrap_or(2u64 << tries).min(15))
}

async fn get<T: serde::de::DeserializeOwned>(path: &str, query: &[(&str, String)]) -> Result<T, String> {
    let qs: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", crate::install::encode_segment(k), crate::install::encode_segment(v)))
        .collect();
    let url = format!("{API}/{path}?{}", qs.join("&"));
    // no máximo MAX_INFLIGHT pedidos simultâneos no total, qualquer que seja o número de varreduras
    let _permit = INFLIGHT.acquire().await.map_err(|e| e.to_string())?;
    let mut tries = 0u32;
    let resp = loop {
        let resp = HTTP_JSON
            .get(&url)
            .header("User-Agent", UA)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("Falha de rede: {e}"))?;
        // 429: até MAX_RETRIES novas tentativas com pausa crescente; o permit fica retido, então os demais pedidos também esperam
        if resp.status().as_u16() == 429 && tries < MAX_RETRIES {
            let after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
            tokio::time::sleep(backoff(tries, after)).await;
            tries += 1;
            continue;
        }
        break resp;
    };
    if !resp.status().is_success() {
        return Err(format!("GameBanana respondeu HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("Resposta inválida: {e}"))
}

/// Resolve a entrada do GameBanana para Switch quando o título inclui o sufixo da plataforma.
fn match_game(games: &[GameRec], name: &str) -> Option<u64> {
    let want = catalog::norm(name);
    games
        .iter()
        .find(|g| catalog::norm(&g.name) == want)
        .or_else(|| {
            games.iter().find(|g| {
                let candidate = catalog::norm(&g.name);
                ["nintendoswitch", "switch"]
                    .iter()
                    .any(|suffix| candidate.strip_suffix(suffix) == Some(want.as_str()))
            })
        })
        .map(|g| g.id)
}

/// Id do jogo no GameBanana; prefere nome exato e aceita o sufixo da plataforma Switch.
async fn find_game(name: &str) -> Result<Option<u64>, String> {
    let want = catalog::norm(name);
    if let Some(&id) = GAME_IDS.lock().get(&want) {
        return Ok(Some(id));
    }
    let q = [
        ("_sSearchString", name.to_string()),
        ("_sModelName", "Game".into()),
        ("_nPerpage", "50".into()),
    ];
    let page: Page<GameRec> = get("Util/Search/Results", &q).await?;
    let found = match_game(&page.records, name);
    if let Some(id) = found {
        GAME_IDS.lock().insert(want, id);
    }
    Ok(found)
}

/// Uma página do índice de mods do jogo, ordenada por `sort` (decrescente).
async fn fetch_page(game: u64, featured: bool, page: u32, sort: &'static str) -> Result<Page<Rec>, String> {
    let mut q = vec![
        ("_nPage", page.to_string()),
        ("_nPerpage", PER_PAGE.to_string()),
        ("_sSort", sort.into()),
        ("_aFilters[Generic_Game]", game.to_string()),
    ];
    if featured {
        q.push(("_aFilters[Generic_WasFeatured]", "true".into()));
    }
    get("Mod/Index", &q).await
}

// jogo -> ids em ordem de mais baixados; o índice não traz `_nDownloadCount`, só aceita ordenar por ele
static TOP_DL: LazyLock<Mutex<HashMap<u64, (Instant, Vec<u64>)>>> = LazyLock::new(Default::default);
const TOP_DL_PAGES: u32 = 4;

/// Ids dos mods mais baixados do jogo (até `TOP_DL_PAGES * PER_PAGE`), do primeiro para o último.
pub async fn top_downloads(name: &str) -> Result<Vec<u64>, String> {
    let Some(game) = find_game(name).await? else { return Ok(Vec::new()) };
    if let Some((at, ids)) = TOP_DL.lock().get(&game) {
        if at.elapsed() < TTL { return Ok(ids.clone()); }
    }
    let jobs: Vec<_> = (1..=TOP_DL_PAGES)
        .map(|p| tauri::async_runtime::spawn(fetch_page(game, false, p, "Generic_MostDownloaded")))
        .collect();
    let mut ids = Vec::new();
    for job in jobs {
        let p = job.await.map_err(|e| format!("Falha de rede: {e}"))??;
        ids.extend(p.records.into_iter().filter(|r| r.has_files && !r.obsolete).map(|r| r.id));
    }
    TOP_DL.lock().insert(game, (Instant::now(), ids.clone()));
    Ok(ids)
}

/// Busca um lote contíguo de páginas e informa se atingiu o fim ou o corte de likes.
async fn fetch_chunk(
    game: u64,
    featured: bool,
    min_likes: u32,
    start_page: u32,
    count: u32,
) -> Result<(BTreeMap<u64, Rec>, bool), String> {
    let mut out = BTreeMap::new();
    let end = start_page + count;
    let jobs: Vec<_> = (start_page..end)
        .map(|p| tauri::async_runtime::spawn(fetch_page(game, featured, p, "Generic_MostLiked")))
        .collect();
    let mut complete = false;
    for job in jobs {
        let p = job.await.map_err(|e| format!("Falha de rede: {e}"))??;
        let empty = p.records.is_empty();
        for r in p.records {
            if r.likes < min_likes {
                complete = true;
                return Ok((out, complete));
            }
            out.insert(r.id, r);
        }
        if p.meta.complete || empty {
            complete = true;
        }
    }
    Ok((out, complete))
}

/// Mods do jogo por curtidas (decrescente), `BATCH` páginas por vez; para ao cair abaixo de `min_likes`.
async fn fetch(game: u64, featured: bool, min_likes: u32, max_pages: u32) -> Result<BTreeMap<u64, Rec>, String> {
    let mut out = BTreeMap::new();
    let mut page = 1;
    while page <= max_pages {
        let count = BATCH.min(max_pages - page + 1);
        let (chunk, complete) = fetch_chunk(game, featured, min_likes, page, count).await?;
        out.extend(chunk);
        if complete {
            break;
        }
        page += count;
    }
    Ok(out)
}

/// Procura um mod do GameBanana em qualquer cache existente em memória.
pub fn find_cached_mod(id: &str) -> Option<ModEntry> {
    let c = CACHE.lock();
    for (_, entry) in c.iter() {
        if let Some(m) = entry.mods.iter().find(|m| m.entry.id == id) {
            return Some(m.entry.clone());
        }
    }
    None
}

/// Lista em cache e ainda válida.
/// Curados só podem ser derivados do cache de "todos" se a busca de "todos" estiver COMPLETA,
/// evitando que um stream parcial de "todos" mascare os curados no prefetch.
fn cached(tid: &str, all: bool) -> Option<Vec<GbMod>> {
    let c = CACHE.lock();
    let entry = c.get(&(tid.to_string(), all)).filter(|e| e.at.elapsed() < TTL);
    if let Some(e) = entry {
        if !all || e.complete {
            return Some(e.mods.clone());
        }
    }
    if all {
        return None;
    }
    c.get(&(tid.to_string(), true))
        .filter(|e| e.complete && e.at.elapsed() < TTL)
        .map(|e| e.mods.iter().filter(|m| m.likes >= MIN_LIKES || m.featured).cloned().collect())
}

#[derive(Serialize, Clone)]
pub struct GbMod {
    #[serde(flatten)]
    pub entry: ModEntry,
    pub thumb: Option<String>,
    pub likes: u32,
    pub views: u32,
    pub featured: bool,
    pub nsfw: bool,
    pub category: Option<String>,
}
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GbMore {
    pub tid: String,
    pub all: bool,
    pub request_id: u64,
    pub mods: Vec<GbMod>,
    pub complete: bool,
}

#[derive(Serialize)]
pub struct GbList {
    pub found: bool,
    pub mods: Vec<GbMod>,
}
/// Primeira imagem do mod com algum dos tamanhos `keys` (em ordem de preferência);
/// _sFile530/_sFile800 só existem na primeira imagem, as demais trazem apenas _sFile100.
fn image(media: &serde_json::Value, keys: &[&str]) -> Option<String> {
    media["_aImages"].as_array()?.iter().find_map(|i| {
        let file = keys.iter().find_map(|k| i[*k].as_str())?;
        Some(format!("{}/{}", i["_sBaseUrl"].as_str()?, file))
    })
}

/// Mods do jogo `tid`/`name`. `all` = tudo que o site tem; senão só os curados.
/// Resultado fica em memória por `TTL`; `fresh` ignora o cache (botão de atualizar).
fn to_gb_mods(recs: BTreeMap<u64, Rec>, tid: &str) -> Vec<GbMod> {
    let mut mods: Vec<_> = recs
        .into_values()
        .filter(|r| r.has_files && !r.obsolete)
        .map(|r| {
            let n_lower = r.name.to_lowercase();
            let nsfw = r.has_content_ratings
                || n_lower.contains("nsfw")
                || n_lower.contains("+18")
                || n_lower.contains("18+")
                || n_lower.contains("nude");
            let category = r.root_category.get("_sName").and_then(|v| v.as_str()).map(String::from);
            GbMod {
                thumb: image(&r.media, &["_sFile530", "_sFile100"]),
                likes: r.likes,
                views: r.views,
                featured: r.featured,
                nsfw,
                category,
                entry: ModEntry {
                    id: format!("{}{}", Source::Gamebanana.id_prefix(), r.id),
                    tid: Some(tid.to_string()),
                    name: r.name,
                    version: None,
                    kind: ModKind::Archive,
                    files: vec![ModFile { src: r.id.to_string(), dest: String::new() }],
                    size: 0,
                    group: String::new(),
                    source: Source::Gamebanana,
                },
            }
        })
        .collect();
    mods.sort_by(|a, b| b.likes.cmp(&a.likes).then_with(|| a.entry.name.cmp(&b.entry.name)));
    mods
}

/// Busca mods do GameBanana sem associar eventos progressivos a um pedido da interface.
pub async fn list(
    app: Option<&AppHandle>,
    tid: &str,
    name: &str,
    all: bool,
    fresh: bool,
) -> Result<GbList, String> {
    list_for_request(app, tid, name, all, fresh, 0).await
}

/// Mods do jogo `tid`/`name`. `all` = todos (com entrega progressiva); senão, só os curados.
/// No modo "Todos", a primeira página retorna imediatamente; as demais continuam em segundo plano.
pub async fn list_for_request(
    app: Option<&AppHandle>,
    tid: &str,
    name: &str,
    all: bool,
    fresh: bool,
    request_id: u64,
) -> Result<GbList, String> {
    if !fresh {
        if let Some(mods) = cached(tid, all) {
            return Ok(GbList { found: true, mods });
        }
    }
    let Some(game) = find_game(name).await? else {
        return Ok(GbList { found: false, mods: Vec::new() });
    };

    if !all {
        let featured = tauri::async_runtime::spawn(fetch(game, true, 0, MAX_PAGES));
        let mut r = fetch(game, false, MIN_LIKES, MAX_PAGES).await?;
        r.extend(featured.await.map_err(|e| format!("Falha de rede: {e}"))??);
        let mods = to_gb_mods(r, tid);
        CACHE.lock().insert((tid.to_string(), false), CacheEntry {
            at: Instant::now(),
            mods: mods.clone(),
            complete: true,
        });
        return Ok(GbList { found: true, mods });
    }

    // Mostra a primeira página (até 50 mods) sem esperar pelos próximos lotes.
    let (mut all_recs, complete) = fetch_chunk(game, false, 0, 1, 1).await?;
    let initial_mods = to_gb_mods(all_recs.clone(), tid);
    CACHE.lock().insert((tid.to_string(), true), CacheEntry {
        at: Instant::now(),
        mods: initial_mods.clone(),
        complete,
    });

    // Se houver mais páginas a buscar, continua em segundo plano alimentando o cache e a UI
    if !complete && app.is_some() {
        let app_handle = app.cloned().unwrap();
        let tid_owned = tid.to_string();
        tauri::async_runtime::spawn(async move {
            let mut page = 2;
            while page <= MAX_PAGES {
                let count = BATCH.min(MAX_PAGES - page + 1);
                let Ok((chunk, is_complete)) = fetch_chunk(game, false, 0, page, count).await else { break };
                if chunk.is_empty() {
                    CACHE.lock().insert((tid_owned.clone(), true), CacheEntry {
                        at: Instant::now(),
                        mods: to_gb_mods(all_recs.clone(), &tid_owned),
                        complete: true,
                    });
                    break;
                }
                all_recs.extend(chunk);
                let current_mods = to_gb_mods(all_recs.clone(), &tid_owned);
                let is_last = is_complete || page + count > MAX_PAGES;
                CACHE.lock().insert((tid_owned.clone(), true), CacheEntry {
                    at: Instant::now(),
                    mods: current_mods.clone(),
                    complete: is_last,
                });
                let _ = app_handle.emit(
                    "gamebanana-more",
                    GbMore {
                        tid: tid_owned.clone(),
                        request_id,
                        all: true,
                        mods: current_mods,
                        complete: is_last,
                    },
                );
                if is_complete {
                    break;
                }
                page += count;
            }
        });
    }

    Ok(GbList { found: true, mods: initial_mods })
}

/// Pré-carrega em segundo plano os mods curados de todos os jogos do usuário.
/// Já cacheados são ignorados; a concorrência HTTP continua limitada por `MAX_INFLIGHT`.
pub async fn prefetch_curated(games: Vec<(String, String)>) {
    for batch in games.chunks(PREFETCH_GAMES) {
        let jobs: Vec<_> = batch
            .iter()
            .filter(|(tid, _)| cached(tid, false).is_none())
            .cloned()
            .map(|(tid, name)| {
                tauri::async_runtime::spawn(async move {
                    let _ = list(None, &tid, &name, false, false).await;
                })
            })
            .collect();
        for job in jobs {
            let _ = job.await;
        }
    }
}

/// Dados da página do mod para o popup.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GbDetail {
    pub text: String,
    pub image: Option<String>,
    pub submitter: Option<String>,
    pub version: Option<String>,
    pub downloads: u64,
    pub size: u64,
    pub updated: u64,
}

pub async fn detail(id: u64) -> Result<GbDetail, String> {
    let props = "_sText,_sVersion,_tsDateUpdated,_tsDateModified,_nDownloadCount,_aSubmitter,_aPreviewMedia,_aFiles";
    let v: serde_json::Value = get(&format!("Mod/{id}"), &[("_csvProperties", props.into())]).await?;
    let latest = v["_aFiles"].as_array().and_then(|f| f.iter().max_by_key(|f| f["_tsDateAdded"].as_u64().unwrap_or(0)));
    Ok(GbDetail {
        text: v["_sText"].as_str().unwrap_or_default().to_string(),
        image: image(&v["_aPreviewMedia"], &["_sFile800", "_sFile530", "_sFile100"]),
        submitter: v["_aSubmitter"]["_sName"].as_str().map(str::to_string),
        version: v["_sVersion"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
        downloads: v["_nDownloadCount"].as_u64().unwrap_or(0),
        size: latest.and_then(|f| f["_nFilesize"].as_u64()).unwrap_or(0),
        updated: v["_tsDateUpdated"].as_u64().or_else(|| v["_tsDateModified"].as_u64()).unwrap_or(0),
    })
}

#[derive(Deserialize)]
struct Files {
    #[serde(rename = "_aFiles", default)]
    files: Vec<File>,
}

#[derive(Deserialize)]
struct File {
    #[serde(rename = "_sFile")]
    name: String,
    #[serde(rename = "_sDownloadUrl")]
    url: String,
    #[serde(rename = "_tsDateAdded", default)]
    added: u64,
}

fn select_download(files: Vec<File>) -> Option<(String, String)> {
    files
        .into_iter()
        .filter_map(|f| {
            let ext = f.name.rsplit_once('.')?.1.to_lowercase();
            let unsupported = f.name.to_ascii_lowercase().ends_with("_tkcl.zip");
            (!unsupported && matches!(ext.as_str(), "zip" | "7z" | "rar"))
                .then_some((f.added, f.url, ext))
        })
        .max_by_key(|(added, ..)| *added)
        .map(|(_, url, ext)| (url, ext))
}

/// URL e extensão do arquivo instalável mais recente do mod.
pub async fn download_url(mod_id: &str) -> Result<(String, String), String> {
    let f: Files = get(&format!("Mod/{mod_id}"), &[("_csvProperties", "_aFiles".into())]).await?;
    select_download(f.files)
        .ok_or_else(|| "Nenhum pacote compatível neste mod do GameBanana".into())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_honors_retry_after_and_is_capped() {
        let secs = |t, a| backoff(t, a).as_secs();
        assert_eq!((secs(0, None), secs(1, None), secs(2, None)), (2, 4, 8));
        assert_eq!(secs(0, Some(7)), 7);
        assert_eq!(secs(0, Some(600)), 15);
    }

    /// Registro mínimo (campos opcionais ausentes) e extras desconhecidos não podem quebrar o parse.
    #[test]
    fn parses_sparse_index_page() {
        let j = r#"{"_aMetadata":{"_nRecordCount":2,"_bIsComplete":true,"_nPerpage":50},
            "_aRecords":[{"_idRow":1,"_sName":"A","_bHasFiles":true,"_nLikeCount":7,"_bWasFeatured":true,"_bHasContentRatings":true,"_aRootCategory":{"_sName":"Skins"},"_x":{},"_aPreviewMedia":{"_aImages":[{"_sBaseUrl":"https://x/ss","_sFile100":"100-a.jpg"}]}},
                         {"_idRow":2,"_sName":"B","_aPreviewMedia":[]}]}"#;
        let p: Page<Rec> = serde_json::from_str(j).unwrap();
        assert!(p.meta.complete);
        assert_eq!((p.records[0].likes, p.records[0].featured, p.records[0].has_files, p.records[0].has_content_ratings), (7, true, true, true));
        assert_eq!(p.records[0].root_category["_sName"].as_str(), Some("Skins"));
        assert_eq!((p.records[1].likes, p.records[1].has_files, p.records[1].obsolete, p.records[1].has_content_ratings), (0, false, false, false));
        assert!(p.records[1].root_category.is_null());
        let keys = ["_sFile530", "_sFile100"];
        assert_eq!(image(&p.records[0].media, &keys), Some("https://x/ss/100-a.jpg".into()));
        assert_eq!(image(&p.records[1].media, &keys), None);
        // tamanho maior tem preferência quando existe
        let m = serde_json::json!({"_aImages": [{"_sBaseUrl": "https://x", "_sFile100": "s.jpg", "_sFile530": "b.jpg"}]});
        assert_eq!(image(&m, &keys), Some("https://x/b.jpg".into()));
    }

    #[test]
    fn selects_layeredfs_archive_instead_of_tkmm_container() {
        let files: Files = serde_json::from_str(
            r#"{"_aFiles":[
                {"_sFile":"tkmm_version_-_infinite_rocket_shield_tkcl.zip","_sDownloadUrl":"https://gamebanana.com/dl/1584175","_tsDateAdded":1766241688,"_sDescription":"Use this for TKMM - includes both versions of the mod"},
                {"_sFile":"infinite_rocket_shield_efe82.7z","_sDownloadUrl":"https://gamebanana.com/dl/1581292","_tsDateAdded":1765825170,"_sDescription":"Infinite rocket all the time"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            select_download(files.files),
            Some(("https://gamebanana.com/dl/1581292".into(), "7z".into()))
        );
    }

    #[test]
    fn rejects_tkmm_container_without_installable_alternative() {
        let files: Files = serde_json::from_str(
            r#"{"_aFiles":[{"_sFile":"tkmm_version_-_infinite_rocket_shield_tkcl.zip","_sDownloadUrl":"https://gamebanana.com/dl/1584175","_tsDateAdded":1766241688,"_sDescription":"Use this for TKMM"}]}"#,
        )
        .unwrap();
        assert_eq!(select_download(files.files), None);
    }

    #[test]
    fn keeps_layeredfs_archive_with_tkmm_description() {
        let files: Files = serde_json::from_str(
            r#"{"_aFiles":[
                {"_sFile":"layeredfs_compatible.zip","_sDownloadUrl":"https://gamebanana.com/dl/1584000","_tsDateAdded":1766241688,"_sDescription":"Not for TKMM; use LayeredFS"},
                {"_sFile":"older_layeredfs.7z","_sDownloadUrl":"https://gamebanana.com/dl/1583999","_tsDateAdded":1766241600,"_sDescription":"LayeredFS package"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            select_download(files.files),
            Some(("https://gamebanana.com/dl/1584000".into(), "zip".into()))
        );
    }

    /// Zelda TotK: curados vêm com id, e o mod mais curtido resolve um download.
    #[test]
    #[ignore]
    fn gamebanana_network() {
        tauri::async_runtime::block_on(async {
            let t0 = Instant::now();
            let curated = list(None, "0100F2C0115B6000", "The Legend of Zelda: Tears of the Kingdom", false, true).await.unwrap();
            println!("curados em {:?}", t0.elapsed());
            let t0 = Instant::now();
            let all = list(None, "0100F2C0115B6000", "The Legend of Zelda: Tears of the Kingdom", true, true).await.unwrap();
            println!("curados {} / todos {} (todos em {:?})", curated.mods.len(), all.mods.len(), t0.elapsed());
            assert!(curated.found && !curated.mods.is_empty() && all.found && !all.mods.is_empty());
            // curados saem do cache de "todos" sem rede e batem com a busca direta
            let t0 = Instant::now();
            let derived = list(None, "0100F2C0115B6000", "x", false, false).await.unwrap();
            assert!(t0.elapsed() < Duration::from_millis(50));
            assert_eq!(derived.mods.len(), curated.mods.len());
            let top = &curated.mods[0];
            let (url, ext) = download_url(&top.entry.files[0].src).await.unwrap();
            println!("{} {url} {ext}", top.entry.name);
            assert!(url.starts_with("https://gamebanana.com/dl/"));
        });
    }

    #[test]
    fn prefetch_curated_skips_cached() {
        tauri::async_runtime::block_on(async {
            let tid = "0100TEST00000000";
            CACHE.lock().insert((tid.to_string(), false), CacheEntry {
                at: Instant::now(),
                mods: Vec::new(),
                complete: true,
            });
            let t0 = Instant::now();
            prefetch_curated(vec![(tid.to_string(), "Nonexistent Game".into())]).await;
            assert!(t0.elapsed() < Duration::from_millis(50));
        });
    }

    #[test]
    fn cached_curated_does_not_derive_from_incomplete_all_cache() {
        let tid = "0100INCOMPLETE000";
        CACHE.lock().insert(
            (tid.to_string(), true),
            CacheEntry {
                at: Instant::now(),
                mods: Vec::new(),
                complete: false,
            },
        );
        // Cache de "all" ainda está parcial: Curados NÃO podem derivar dele
        assert!(cached(tid, false).is_none());

        // Quando "all" conclui e marca `complete: true`:
        CACHE.lock().insert(
            (tid.to_string(), true),
            CacheEntry {
                at: Instant::now(),
                mods: Vec::new(),
                complete: true,
            },
        );
        // Agora pode derivar com segurança
        assert!(cached(tid, false).is_some());
    }
    #[test]
    fn resolves_switch_variant_when_gamebanana_adds_platform_suffix() {
        let games = vec![
            GameRec {
                id: 5866,
                name: "The Legend of Zelda: Breath of the Wild (WiiU)".into(),
            },
            GameRec {
                id: 6386,
                name: "The Legend of Zelda: Breath of the Wild (Switch)".into(),
            },
        ];
        assert_eq!(
            match_game(&games, "The Legend of Zelda: Breath of the Wild"),
            Some(6386)
        );
    }

    #[test]
    fn exact_game_name_precedes_platform_variant() {
        let games = vec![
            GameRec {
                id: 1,
                name: "The Legend of Zelda: Breath of the Wild (Switch)".into(),
            },
            GameRec {
                id: 2,
                name: "The Legend of Zelda: Breath of the Wild".into(),
            },
        ];
        assert_eq!(
            match_game(&games, "The Legend of Zelda: Breath of the Wild"),
            Some(2)
        );
    }

}
