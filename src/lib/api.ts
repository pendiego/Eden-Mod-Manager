import { invoke } from "@tauri-apps/api/core";

export type Source = "official" | "theboy181" | "wiki" | "ptbr" | "gamebanana";
// Espelha catalog.rs: repositório, branch e prefixo do id de cada fonte.
const SOURCES: Record<Source, { repo: string; branch: string; label: string }> = {
  official: { repo: "ADEMOLA200/Switch-Emulator-Mod-Database", branch: "develop", label: "" },
  theboy181: { repo: "theboy181/switch-ptchtxt-mods", branch: "main", label: "TheBoy181" },
  wiki: { repo: "amakvana/Switch-Mods-Wiki-Archive", branch: "main", label: "Wiki" },
  ptbr: { repo: "staticpiratex/Traducoes-SWITCH-PTBR", branch: "NintendoSwitch", label: "PT-BR" },
  gamebanana: { repo: "", branch: "", label: "GameBanana" },
};
export const SOURCE_LABEL: Record<Source, string> = {
  official: SOURCES.official.label,
  theboy181: SOURCES.theboy181.label,
  wiki: SOURCES.wiki.label,
  ptbr: SOURCES.ptbr.label,
  gamebanana: SOURCES.gamebanana.label,
};

export type GbMod = ModEntry & { thumb: string | null; likes: number; views: number; featured: boolean; nsfw?: boolean; category?: string | null };
export type GbList = { found: boolean; mods: GbMod[] };
export type GbMore = { tid: string; all: boolean; requestId: number; mods: GbMod[]; complete: boolean };
export type GbDetail = { text: string; image: string | null; submitter: string | null; version: string | null; downloads: number; size: number; updated: number };
export type FrameworkStatus = { needed: boolean; skyline: boolean; arcropolis: boolean | null };
export type ModFile = { src: string; dest: string };
export type ModEntry = {
  id: string;
  tid: string | null;
  name: string;
  version: string | null;
  kind: "archive" | "files" | "pack";
  files: ModFile[];
  size: number;
  group: string;
  source: Source;
};
export type Catalog = {
  treeSha: string;
  fetchedAt: number;
  mods: ModEntry[];
  names: Record<string, string>;
};
export type Game = {
  tid: string;
  name: string | null;
  version: string | null;
  icon: string | null;
  isCompressed: boolean;
  updateFile: string | null;
  updateRegistered: boolean;
};
export type Destination = "emulator" | "arcropolis" | "save";
export type RootInfo = { key: string; name: string; fileCount: number; destination?: Destination };
export type Prepared = { token: string; roots: RootInfo[]; isSave?: boolean };
export type Installed = {
  tid: string;
  folder: string;
  destination?: Destination;
  modId: string;
  rootKey: string;
  name: string;
  version: string | null;
  installedAt: number;
};
export type InstalledView = Installed & { enabled: boolean };
export type Conflict = { folders: string[]; count: number; sample: string };

export type Emu = "eden" | "yuzu" | "ryujinx";
export type RomFile = { path: string; name: string; ext: string; size: number };
export type NszOp = "compress" | "decompress" | "verify" | "info";
export type NszResult = { ok: boolean; log: string; output: string | null; outputSize: number | null };
export type UpdateCheck = { current: string; version: string | null; notes: string | null };
export type EmuDir = { kind: Emu; dir: string | null };
export type StorageInfo = { cacheBytes: number; toolsBytes: number };

export const norm = (s: string) =>
  s
    .replace(/\s*-\s*\d+$/, "")
    .toLowerCase()
    .replace(/[^a-z0-9]/g, "");

/** Caminho no repositório (o id das fontes extras tem o prefixo "<fonte>:"). */
export const modPath = (m: ModEntry) => (m.source === "official" ? m.id : m.id.slice(m.source.length + 1));

export const githubUrl = (m: ModEntry) =>
  m.source === "gamebanana"
    ? `https://gamebanana.com/mods/${modPath(m)}`
    : m.source === "ptbr"
    ? `https://github.com/${SOURCES.ptbr.repo}/releases/tag/${SOURCES.ptbr.branch}`
    : `https://github.com/${SOURCES[m.source].repo}/tree/${SOURCES[m.source].branch}/${modPath(m)
        .split("/")
        .map(encodeURIComponent)
        .join("/")}`;

export const api = {
  getEmu: () => invoke<{ kind: Emu; dir: string | null }>("get_emu"),
  setEmulator: (kind: Emu) => invoke<void>("set_emulator", { kind }),
  setEmuDir: (kind: Emu, path: string) => invoke<void>("set_emu_dir", { kind, path }),
  getCatalog: (force: boolean) => invoke<Catalog>("get_catalog", { force }),
  gamebananaMods: (tid: string, name: string, all: boolean, requestId: number, fresh = false) =>
    invoke<GbList>("gamebanana_mods", { tid, name, all, fresh, requestId }),
  gamebananaDetail: (id: number) => invoke<GbDetail>("gamebanana_detail", { id }),
  gamebananaTopDownloads: (name: string) => invoke<number[]>("gamebanana_top_downloads", { name }),
  listGames: () => invoke<Game[]>("list_games"),
  gameCover: (tid: string) => invoke<string | null>("game_cover", { tid }),
  listInstalled: (tid: string) => invoke<InstalledView[]>("list_installed", { tid }),
  setModEnabled: (tid: string, folder: string, enabled: boolean) =>
    invoke<void>("set_mod_enabled", { tid, folder, enabled }),
  installFrameworks: (tid: string) => invoke<void>("install_frameworks", { tid }),
  frameworkStatus: (tid: string) => invoke<FrameworkStatus>("framework_status", { tid }),
  emuExe: () => invoke<string | null>("emu_exe"),
  setEmuExe: (path: string) => invoke<void>("set_emu_exe", { path }),
  setGameFile: (tid: string, path: string) => invoke<void>("set_game_file", { tid, path }),
  launchGame: (tid: string) => invoke<void>("launch_game", { tid }),
  listConflicts: (tid: string) => invoke<Conflict[]>("list_conflicts", { tid }),
  checkUpdate: () => invoke<UpdateCheck>("check_update"),
  installUpdate: () => invoke<void>("install_update"),
  prepareLocal: (tid: string, path: string) => invoke<Prepared>("prepare_local", { tid, path }),
  getEmuDirs: () => invoke<EmuDir[]>("get_emu_dirs"),
  storageInfo: () => invoke<StorageInfo>("storage_info"),
  clearCache: () => invoke<void>("clear_cache"),
  removeTools: () => invoke<void>("remove_tools"),
  prepareInstall: (tid: string, modId: string) => invoke<Prepared>("prepare_install", { tid, modId }),
  commitInstall: (token: string, keys: string[]) => invoke<Installed[]>("commit_install", { token, keys }),
  cancelInstall: (token: string) => invoke<void>("cancel_install", { token }),
  uninstall: (tid: string, folder: string) => invoke<void>("uninstall", { tid, folder }),
  openModFolder: (tid: string) => invoke<void>("open_mod_folder", { tid }),
  listRoms: () => invoke<RomFile[]>("list_roms"),
  nszRun: (op: NszOp, path: string, deleteSource: boolean) =>
    invoke<NszResult>("nsz_run", { op, path, deleteSource }),
  nszCanVerify: () => invoke<boolean>("nsz_can_verify"),
  peekArchive: (modId: string) => invoke<RootInfo[]>("peek_archive", { modId }),
  registerGameUpdate: (tid: string) => invoke<string | null>("register_game_update", { tid }),
  prefetchGamebanana: (games: { tid: string; name: string | null }[]) =>
    invoke<void>("prefetch_gamebanana", { games }),
};
