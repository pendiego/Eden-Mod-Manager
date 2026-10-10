<script lang="ts">
  import "@fontsource-variable/geist";
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { getVersion } from "@tauri-apps/api/app";
  import { open, confirm } from "@tauri-apps/plugin-dialog";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import {
    api, githubUrl, modPath, SOURCE_LABEL, norm,
    type Catalog, type Conflict, type Game, type InstalledView, type ModEntry, type NszOp, type Prepared, type RomFile,
    type Emu, type RootInfo, type Source, type UpdateCheck, type EmuDir, type StorageInfo,
    type GbMod, type GbMore, type GbDetail, type FrameworkStatus,
  } from "$lib/api";
  import { EMU_HINT, EMU_NAME, i18n, locale, setLang, t, trErr, type Lang } from "$lib/i18n.svelte";

  let emu = $state<Emu>("eden");
  let emuDir = $state<string | null>(null);
  let catalog = $state<Catalog | null>(null);
  let games = $state<Game[]>([]);
  let selected = $state<Game | null>(null);
  let installed = $state<InstalledView[]>([]);
  let conflicts = $state<Conflict[]>([]);
  let gameFilter = $state("");
  let contents = $state<Record<string, RootInfo[]>>({});
  const PEEK_MAX = 30 * 1048576;
  let search = $state("");
  let srcFilter = $state<Source | "all">("all");
  let view = $state<"repo" | "gb">("repo");
  let onlyMatch = $state(false);
  let error = $state("");
  let notice = $state("");
  let busy = $state(false);
  let loadingCatalog = $state(false);
  let fw = $state<FrameworkStatus | null>(null);
  const smash = $derived(selected?.tid.toUpperCase() === "01006A800016E000");
  const fwMissing = $derived(fw ? [!fw.skyline && "Skyline", fw.arcropolis === false && "ARCropolis"].filter(Boolean).join(" + ") : "");
  let progress = $state<{ received: number; total: number | null } | null>(null);
  let prepared = $state<Prepared | null>(null);
  let dlg = $state<HTMLDialogElement>();
  let savePrepared = $state<Prepared | null>(null);
  let saveDlg = $state<HTMLDialogElement>();
  let removing = $state<InstalledView | null>(null);
  let removeDlg = $state<HTMLDialogElement>();
  let guide = $state<HTMLDialogElement>();
  let dependencies = $state<HTMLDialogElement>();
  let upd = $state<HTMLDialogElement>();
  let update = $state<UpdateCheck | null>(null);
  let sett = $state<HTMLDialogElement>();
  let appVersion = $state("");
  let emuDirs = $state<EmuDir[]>([]);
  let storage = $state<StorageInfo | null>(null);
  let autoUpdate = $state(localStorage.getItem("autoUpdate") !== "off");
  let gbAll = $state(localStorage.getItem("gbAll") === "on");
  let gb = $state<{ tid: string; status: "loading" | "ok" | "notfound" | "error"; mods: GbMod[]; error: string } | null>(null);
  const gbPendingMore = new Map<number, GbMore>();
  let gbSort = $state<"likes" | "newest" | "views" | "downloads" | "name">("likes");
  // posição de cada mod no ranking de downloads, por jogo (só os mais baixados; o resto fica depois, por curtidas)
  let gbDlRanks = $state<Record<string, Map<string, number>>>({});
  const gbDlPending = new Set<string>();
  const gbDlFailed = new Set<string>();
  let gbFeaturedOnly = $state(false);
  let gbCategory = $state<string>("all");
  let hideNsfw = $state(true);
  onMount(() => {
    const savedNsfw = localStorage.getItem("hideNsfw");
    if (savedNsfw !== null) hideNsfw = savedNsfw !== "0";
  });

  async function loadDownloadRank(game: Game) {
    if (!game.name || gbDlRanks[game.tid] || gbDlPending.has(game.tid) || gbDlFailed.has(game.tid)) return;
    gbDlPending.add(game.tid);
    // a resposta é gravada sob o TID do pedido: trocar de jogo no meio não mistura rankings
    const ids = await run(() => api.gamebananaTopDownloads(game.name!));
    gbDlPending.delete(game.tid);
    if (ids) gbDlRanks = { ...gbDlRanks, [game.tid]: new Map(ids.map((id, i) => [String(id), i])) };
    else gbDlFailed.add(game.tid);
  }

  function sortByDownloads() {
    gbSort = "downloads";
    if (selected) gbDlFailed.delete(selected.tid); // clicar de novo tenta outra vez após falha
  }

  // ordenação ativa + jogo selecionado => garante o ranking daquele jogo, sem depender do clique
  $effect(() => {
    if (view === "gb" && gbSort === "downloads" && selected) void loadDownloadRank(selected);
  });

  async function toggleNsfw(e?: Event) {
    const target = (e?.currentTarget as HTMLElement)?.tagName === "INPUT" ? (e?.currentTarget as HTMLInputElement) : null;
    if (hideNsfw) {
      const ok = await confirm(t("confirmNsfwShow"), {
        title: t("confirmNsfwTitle"),
        kind: "warning",
        okLabel: t("confirmNsfwOk"),
        cancelLabel: t("cancel"),
      });
      if (ok) {
        hideNsfw = false;
        localStorage.setItem("hideNsfw", "0");
      }
      if (target) target.checked = hideNsfw;
    } else {
      hideNsfw = true;
      localStorage.setItem("hideNsfw", "1");
      if (target) target.checked = true;
    }
  }
  let gbShown = $state(50);
  let gbDlg = $state<HTMLDialogElement>();
  let gbOpen = $state<GbMod | null>(null);
  let gbDetail = $state<GbDetail | null>(null);
  let gbDetailErr = $state("");
  let installingId = $state<string | null>(null);
  let failedId = $state<string | null>(null);
  $effect(() => { if (!busy) installingId = null; });
  let micaOn = $state(localStorage.getItem("mica") !== "off");
  let micaOk = $state(false);
  let checked = $state<Record<string, boolean>>({});
  let tab = $state<"mods" | "nsz">("mods");
  let roms = $state<RomFile[]>([]);
  let picked = $state<RomFile[]>([]);
  let deleteSource = $state(false);
  let nszLog = $state("");
  let nszStep = $state<{ percent: number | null; step: string } | null>(null);
  let nszCur = $state<{ path: string; name: string; op: NszOp; stage: string } | null>(null);
  let nszRes = $state<Record<string, { ok: boolean; text: string }>>({});
  let nowMs = $state(0);
  let startMs = 0;
  const fmtDuration = (ms: number) => {
    const s = Math.floor(ms / 1000);
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
  };
  const elapsed = $derived(fmtDuration(nowMs - startMs));
  const runVerb = (op: NszOp) =>
    t(op === "compress" ? "runCompress" : op === "decompress" ? "runDecompress" : op === "verify" ? "runVerify" : "runInfo");
  const stageText = $derived(
    nszCur?.stage === "tool" ? t("stageTool")
    : nszCur?.stage === "legacy" ? t("stageLegacy")
    : nszStep ? `${nszStep.step} ${nszStep.percent?.toLocaleString(locale(), { maximumFractionDigits: 0 }) ?? ""}%`.trim()
    : "",
  );
  $effect(() => {
    if (!nszCur) return;
    const id = setInterval(() => (nowMs = Date.now()), 1000);
    return () => clearInterval(id);
  });
  // mantém o log rolado até a última linha
  function stick(node: HTMLElement, _: string) {
    node.scrollTop = node.scrollHeight;
    return { update: () => (node.scrollTop = node.scrollHeight) };
  }
  let romFilter = $state("");
  const romList = $derived([...picked, ...roms.filter((r) => !picked.some((p) => p.path === r.path))]);
  const shownRoms = $derived.by(() => {
    const q = romFilter.trim().toLowerCase();
    return q ? romList.filter((r) => r.path.toLowerCase().includes(q)) : romList;
  });
  // ícones de traço próprios (viewBox 24); sem biblioteca de ícones instalada
  const ICON = {
    mods: "M4 4h7v7H4z M13 4h7v7h-7z M4 13h7v7H4z M13 13h7v7h-7z",
    nsz: "M3 7h18v13H3z M3 7l2-3h14l2 3 M10 11h4",
    search: "M11 4a7 7 0 1 0 0 14a7 7 0 1 0 0-14z M20 20l-4.2-4.2",
    refresh: "M20 12a8 8 0 1 1-2.3-5.6 M20 4v5h-5",
    folder: "M3 6h6l2 2h10v11H3z",
    plus: "M12 5v14 M5 12h14",
    chev: "M9 6l6 6-6 6",
    play: "M7 4l13 8-13 8z",
    help: "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18z M9.6 9.5a2.5 2.5 0 1 1 3.5 2.3c-.7.4-1.1.9-1.1 1.7 M12 17h.01",
    update: "M12 4v11 M7 10l5 5 5-5 M5 20h14",
    check: "M5 12l5 5 9-10",
    x: "M6 6l12 12 M18 6L6 18",
    alert: "M12 3l9.5 17h-19z M12 10v4 M12 17h.01",
    settings: "M12 15a3 3 0 1 0 0-6a3 3 0 1 0 0 6z M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z",
    trash: "M3 6h18 M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6 M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2 M10 11v6 M14 11v6",
  };
  const initials = (s: string) =>
    s.split(/\s+/).map((w) => w.match(/[\p{L}\p{N}]/u)?.[0] ?? "").filter(Boolean).slice(0, 2).join("").toUpperCase() || "?";
  const cleanName = (n: string) =>
    n.replace(/\s*\[(?:[0-9a-f]{16}|v\d+)\]/gi, "").replace(/\s*\([\d.]+ GB\)/, "").replace(/\.[^.]+$/, "") || n;
  // TID de jogo base termina em 000, update em 800, DLC em qualquer outro sufixo.
  const kindOf = (n: string): "tagBase" | "tagUpdate" | "tagDlc" | null => {
    const tid = n.match(/\b[0-9a-f]{13}([0-9a-f]{3})\b/i)?.[1].toLowerCase();
    if (/dlc/i.test(n)) return "tagDlc";
    return tid === "000" ? "tagBase" : tid === "800" ? "tagUpdate" : tid ? "tagDlc" : null;
  };
  // Agrupa por TID base (updates/DLCs caem no jogo: últimos 13 bits zerados).
  const baseKey = (n: string) => {
    const tid = n.match(/\b[0-9a-f]{16}\b/gi)?.pop();
    return tid ? (BigInt("0x" + tid) & ~0x1fffn).toString(16).padStart(16, "0") : cleanName(n);
  };
  const groups = $derived.by(() => {
    const m = new Map<string, RomFile[]>();
    for (const r of shownRoms) {
      const k = baseKey(r.name);
      m.set(k, [...(m.get(k) ?? []), r]);
    }
    return [...m].map(([key, items]) => {
      const base = items.find((r) => kindOf(r.name) === "tagBase") ?? items[0];
      return {
        key,
        items,
        name: base.name.split(" [")[0].replace(/\.[^.]+$/, ""),
        size: items.reduce((s, r) => s + r.size, 0),
      };
    }).sort((a, b) => a.name.localeCompare(b.name));
  });
  // Capas dos grupos do compressor: reaproveita as dos jogos; o resto é buscado uma vez por sessão.
  let romCovers = $state<Record<string, string>>({});
  const romTried = new Set<string>();
  const romCover = (key: string) => games.find((g) => g.tid === key)?.icon ?? romCovers[key];
  $effect(() => {
    const keys = groups.map((g) => g.key).filter((k) => /^[0-9a-f]{16}$/.test(k) && !romCover(k) && !romTried.has(k));
    keys.forEach((k) => romTried.add(k));
    void (async () => {
      for (const k of keys) {
        const icon = await api.gameCover(k).catch(() => null);
        if (icon) romCovers[k] = icon;
      }
    })();
  });
  const itemName = (r: RomFile, game: string) => {
    const n = cleanName(r.name);
    return n.startsWith(game) ? n.slice(game.length).replace(/^[\s\-\[\]]+|[\s\]]+$/g, "") || n : n;
  };

  $effect(() => {
    if (prepared && !dlg?.open) openModal(dlg);
    else if (!prepared && dlg?.open) dlg.close();
  });

  let canVerify = $state(true);
  async function loadRoms() {
    roms = (await run(api.listRoms)) ?? [];
    canVerify = await api.nszCanVerify().catch(() => true);
  }

  async function showNsz(filter?: string) {
    tab = "nsz";
    if (filter) romFilter = filter;
    await loadRoms();
  }

  async function pickRom() {
    const p = await open({
      multiple: false,
      filters: [{ name: t("switchGames"), extensions: ["nsp", "xci", "nsz", "xcz"] }],
    });
    if (typeof p !== "string" || picked.some((r) => r.path === p)) return;
    const name = p.split(/[\\/]/).pop() ?? p;
    picked = [...picked, { path: p, name, ext: (name.split(".").pop() ?? "").toLowerCase(), size: 0 }];
  }

  async function nsz(op: NszOp, r: RomFile) {
    if (busy) return;
    busy = true;
    notice = "";
    nszLog = "";
    delete nszRes[r.path];
    nszStep = null;
    nszCur = { path: r.path, name: r.name, op, stage: "run" };
    nowMs = startMs = Date.now();
    const del = deleteSource && (op === "compress" || op === "decompress");
    const res = await run(() => api.nszRun(op, r.path, del));
    const failure = error;
    progress = null;
    nszStep = null;
    nszCur = null;
    busy = false;
    if (res) {
      nszLog = res.log;
      const mark = (text: string, ok: boolean) => {
        nszRes[r.path] = { ok, text };
        if (res.output) nszRes[res.output] = { ok, text };
      };
      if (res.ok) {
        const took = fmtDuration(Date.now() - startMs);
        const sizes = res.outputSize && r.size ? `${fmtSize(r.size)} → ${fmtSize(res.outputSize)} · ` : "";
        notice =
          op === "verify" ? t("verifyOk")
          : op === "info" ? t("infoBelow")
          : t("done", { file: res.output?.split(/[\\/]/).pop() ?? r.name });
        mark(op === "verify" ? t("verifyOk") : op === "info" ? t("infoBelow") : `${sizes}${took}`, true);
        if (del) picked = picked.filter((p) => p.path !== r.path);
      } else {
        error = t("nszFailed");
        mark(t("failedShort"), false);
      }
    } else if (failure) {
      nszRes[r.path] = { ok: false, text: failure };
    }
    await loadRoms();
  }

  const compact = (n: number) => n.toLocaleString(locale(), { notation: "compact", maximumFractionDigits: 1 });
  const fmtSize = (n: number) =>
    n >= 1073741824
      ? `${(n / 1073741824).toLocaleString(locale(), { minimumFractionDigits: 1, maximumFractionDigits: 1 })} GB`
      : n >= 1048576
        ? `${Math.round(n / 1048576).toLocaleString(locale())} MB`
        : `${Math.max(1, Math.round(n / 1024)).toLocaleString(locale())} KB`;

  const filteredGames = $derived(
    games.filter((g) =>
      `${g.name ?? ""} ${g.tid}`.toLowerCase().includes(gameFilter.trim().toLowerCase()),
    ),
  );
  const q = $derived(search.trim().toLowerCase());
  const pass = (m: ModEntry, matchOnly = true) =>
    (srcFilter === "all" || m.source === srcFilter) &&
    (!matchOnly || !onlyMatch || !selected?.version || !m.version || m.version === selected.version) &&
    (!q || m.name.toLowerCase().includes(q) || m.id.toLowerCase().includes(q));
  const available = $derived(
    selected && catalog ? catalog.mods.filter((m) => m.tid === selected!.tid && pass(m)) : [],
  );
  const possible = $derived.by(() => {
    if (!selected?.name || !catalog) return [];
    const g = norm(selected.name);
    if (!g) return [];
    return catalog.mods.filter((m) => {
      if (m.tid !== null || !pass(m)) return false;
      const k = norm(m.group);
      return k && (k.includes(g) || g.includes(k));
    });
  });
  // Categorias encontradas nos mods do jogo atual
  const gbCategories = $derived.by(() => {
    if (!gb || !selected || gb.tid !== selected.tid || gb.status !== "ok") return [];
    const set = new Set<string>();
    for (const m of gb.mods) {
      if (m.category) set.add(m.category);
    }
    return Array.from(set).sort();
  });

  const gbCategoryCounts = $derived.by(() => {
    const counts: Record<string, number> = { all: 0 };
    if (!gb || !selected || gb.tid !== selected.tid || gb.status !== "ok") return counts;
    for (const m of gb.mods) {
      if (hideNsfw && m.nsfw) continue;
      if (gbFeaturedOnly && !m.featured) continue;
      counts.all = (counts.all ?? 0) + 1;
      if (m.category) {
        counts[m.category] = (counts[m.category] ?? 0) + 1;
      }
    }
    return counts;
  });

  // Busca e ordenação do GameBanana: palavras-chave, ID, destaque, categoria e ordenação.
  const gbList = $derived.by(() => {
    if (!gb || !selected || gb.tid !== selected.tid || gb.status !== "ok") return [];
    const query = q.trim().toLowerCase();
    const terms = query ? query.split(/\s+/).filter(Boolean) : [];

    const filtered = gb.mods.filter((m) => {
      if (gbFeaturedOnly && !m.featured) return false;
      if (hideNsfw && m.nsfw) return false;
      if (gbCategory !== "all" && m.category !== gbCategory) return false;
      if (!terms.length) return true;
      const name = m.name.toLowerCase();
      const id = m.id.toLowerCase();
      return terms.every((term) => name.includes(term) || id.includes(term));
    });

    switch (gbSort) {
      case "likes":
        filtered.sort((a, b) => b.likes - a.likes || a.name.localeCompare(b.name));
        break;
      case "views":
        filtered.sort((a, b) => b.views - a.views || b.likes - a.likes);
        break;
      case "downloads": {
        const rank = gbDlRanks[selected.tid] ?? new Map<string, number>();
        const pos = (m: GbMod) => rank.get(modPath(m)) ?? Infinity;
        filtered.sort((a, b) => pos(a) - pos(b) || b.likes - a.likes);
        break;
      }
      case "newest": {
        const idNum = (id: string) => parseInt(id.replace(/\D/g, ""), 10) || 0;
        filtered.sort((a, b) => idNum(b.id) - idNum(a.id));
        break;
      }
      case "name":
        filtered.sort((a, b) => a.name.localeCompare(b.name));
        break;
    }

    return filtered;
  });
  const searchHits = $derived.by(() => {
    if (!q || !catalog) return [];
    const listed = new Set([...available, ...possible].map((m) => m.id));
    return catalog.mods.filter((m) => !listed.has(m.id) && pass(m, false));
  });
  const searchResults = $derived(searchHits.slice(0, 200));

  async function run<T>(fn: () => Promise<T>): Promise<T | undefined> {
    error = "";
    try {
      return await fn();
    } catch (e) {
      error = trErr(String(e));
    }
  }

  async function checkUpdate() {
    const r = await run(() => api.checkUpdate());
    if (!r) return;
    if (r.version) { update = r; openModal(upd); } else notice = t("updLatest", { v: r.current });
  }
  async function installUpdate() {
    upd?.close();
    busy = true;
    progress = { received: 0, total: null };
    await run(() => api.installUpdate()); // em caso de sucesso o app fecha/reinicia; daqui pra baixo só em falha
    busy = false;
    progress = null;
  }

  // Capas que o emulador não guardou: busca uma a uma (sem rajada); cada jogo é tentado uma vez por sessão.
  const coverCache = new Map<string, string | null>();
  async function loadCovers() {
    for (const g of games) {
      if (!g.icon && coverCache.get(g.tid)) g.icon = coverCache.get(g.tid)!;
    }
    for (const g of games.filter((g) => !g.icon && !coverCache.has(g.tid))) {
      const icon = await api.gameCover(g.tid).catch(() => null);
      coverCache.set(g.tid, icon);
      const cur = games.find((x) => x.tid === g.tid);
      if (icon && cur) cur.icon = icon;
    }
  }

  async function loadGames() {
    games = (await run(api.listGames)) ?? [];
    if (selected && !games.some((g) => g.tid === selected!.tid)) selected = null;
    void loadCovers();
    if (games.length) void api.prefetchGamebanana(games.map((g) => ({ tid: g.tid, name: g.name }))).catch(() => {});
  }

  async function loadCatalog(force: boolean) {
    busy = true;
    loadingCatalog = true;
    const c = await run(() => api.getCatalog(force));
    loadingCatalog = false;
    busy = false;
    if (c) catalog = c;
    await loadGames();
    if (selected) void loadGb(selected, force);
  }

  // Relê emulador/pasta salvos e recarrega tudo que depende deles.
  async function reloadEmu() {
    const i = await run(api.getEmu);
    emu = i?.kind ?? "eden";
    i18n.emu = EMU_NAME[emu];
    emuDir = i?.dir ?? null;
    selected = null;
    installed = [];
    conflicts = [];
    roms = [];
    picked = [];
    nszLog = "";
    if (!emuDir) games = [];
    else if (catalog) await loadGames();
    else await loadCatalog(false);
  }

  async function switchEmu(k: Emu) {
    if ((await run(() => api.setEmulator(k))) === undefined && error) return;
    tab = "mods";
    await reloadEmu();
  }

  async function changeDir(kind: Emu = emu) {
    const p = await open({ directory: true, title: t("edenDirTitle", { emu: EMU_NAME[kind] }) });
    if (typeof p !== "string") return;
    if ((await run(() => api.setEmuDir(kind, p))) === undefined && error) return;
    if (kind === emu) await reloadEmu();
    if (sett?.open) await refreshSettings();
  }

  function openModal(d?: HTMLDialogElement) { d?.showModal(); d?.focus(); }

  async function openSettings() {
    error = "";
    notice = "";
    openModal(sett);
    appVersion ||= await getVersion();
    await refreshSettings();
  }
  async function refreshSettings() {
    emuDirs = (await run(api.getEmuDirs)) ?? [];
    storage = (await run(api.storageInfo)) ?? null;
  }
  async function clearCache() {
    if ((await run(() => api.clearCache())) === undefined && error) return;
    notice = t("setCleared");
    await refreshSettings();
  }
  async function removeTools() {
    if ((await run(() => api.removeTools())) === undefined && error) return;
    notice = t("setNszRemoved");
    await refreshSettings();
  }
  function applyMica() {
    if (micaOk && micaOn) document.documentElement.dataset.mica = "";
    else delete document.documentElement.dataset.mica;
  }
  function setMica(on: boolean) {
    micaOn = on;
    localStorage.setItem("mica", on ? "on" : "off");
    applyMica();
  }
  function setAutoUpdate(on: boolean) {
    autoUpdate = on;
    localStorage.setItem("autoUpdate", on ? "on" : "off");
  }
  function setGbAll(on: boolean) {
    if (on === gbAll) return;
    gbAll = on;
    gbCategory = "all";
    localStorage.setItem("gbAll", on ? "on" : "off");
    if (selected) void loadGb(selected);
  }

  // GameBanana é buscado por jogo (curados por padrão) e vive fora de `catalog.mods`.
  let gbSeq = 0;
  async function loadGb(g: Game, fresh = false) {
    if (!catalog) return;
    const seq = ++gbSeq;
    gbPendingMore.clear();
    gbShown = 50;
    if (!g.name) { gb = { tid: g.tid, status: "notfound", mods: [], error: "" }; return; }
    gb = { tid: g.tid, status: "loading", mods: [], error: "" };
    try {
      const r = await api.gamebananaMods(g.tid, g.name, gbAll, seq, fresh);
      if (seq !== gbSeq) return;
      const streamed = gbPendingMore.get(seq)?.mods ?? r.mods;
      gbPendingMore.clear();
      gb = { tid: g.tid, status: r.found ? "ok" : "notfound", mods: streamed, error: "" };
    } catch (e) {
      if (seq !== gbSeq) return;
      gb = { tid: g.tid, status: "error", mods: [], error: trErr(String(e)) };
    }
  }

  async function select(g: Game) {
    selected = g;
    gbCategory = "all";
    notice = "";
    await refreshInstalled();
    void loadGb(g);
    peekAll(
      g,
      [...available, ...possible]
        .filter((m) => m.kind === "archive" && m.size <= PEEK_MAX && !contents[m.id])
        .sort((a, b) => a.size - b.size),
    );
  }

  // Lê pacotes do jogo (menores primeiro) com PEEK_WORKERS leituras simultâneas; para se trocar de jogo.
  const PEEK_WORKERS = 3;
  async function peekAll(g: Game, mods: ModEntry[]) {
    let next = 0;
    const worker = async () => {
      while (next < mods.length) {
        const m = mods[next++];
        if (selected?.tid !== g.tid) return;
        try {
          contents[m.id] = await api.peekArchive(m.id);
        } catch {
          // pacote ilegível: fica só com "Instalar", que mostra o erro real
        }
      }
    };
    await Promise.all(Array.from({ length: PEEK_WORKERS }, worker));
    if (!busy) progress = null;
  }

  /** Detecta o Skyline (e no Smash o ARCropolis) do jogo aberto. yuzu não suporta ARCropolis: no Smash fica `null`. */
  async function refreshFw() {
    const tid = selected?.tid;
    const f = tid && !(smash && emu === "yuzu") ? await api.frameworkStatus(tid).catch(() => null) : null;
    if (selected?.tid === tid) fw = f;
  }

  async function refreshInstalled() {
    const tid = selected?.tid;
    void refreshFw();
    installed = tid ? ((await run(() => api.listInstalled(tid))) ?? []) : [];
    const c = tid && installed.length > 1 ? await api.listConflicts(tid).catch(() => []) : [];
    if (selected?.tid === tid) conflicts = c;
  }

  /** Mod do catálogo com versão diferente da instalada (só quando as duas são conhecidas). */
  const newer = (i: InstalledView) => {
    const m = catalog?.mods.find((m) => m.id === i.modId);
    return m?.version && i.version && m.version !== i.version ? m : undefined;
  };

  async function toggle(i: InstalledView) {
    if (busy) return;
    busy = true;
    await run(() => api.setModEnabled(i.tid, i.folder, !i.enabled));
    busy = false;
    progress = null;
    await refreshInstalled();
  }

  async function installFrameworks() {
    const tid = selected?.tid;
    if (busy || !tid) return;
    busy = true;
    notice = "";
    progress = { received: 0, total: null };
    await run(() => api.installFrameworks(tid));
    busy = false;
    progress = null;
    await refreshFw();
    if (!error) notice = t(smash ? "dependenciesReady" : "skylineReady");
  }

  /** Abre o jogo no emulador; se o executável não foi achado sozinho, pede o arquivo uma vez (fica salvo). */
  async function launchGame() {
    const g = selected;
    if (!g || busy) return;
    notice = "";
    if (!(await run(() => api.emuExe()))) {
      const p = await open({ multiple: false, title: t("pickEmuExe", { emu: EMU_NAME[emu] }) });
      if (typeof p !== "string") return;
      await run(() => api.setEmuExe(p));
      if (error) return;
    }
    await run(() => api.launchGame(g.tid));
    // jogo sem TID no nome do arquivo: o usuário aponta o nsp/xci uma vez (fica salvo por jogo)
    if (error === trErr("Arquivo do jogo não encontrado nas pastas de jogos do emulador")) {
      const f = await open({ multiple: false, title: t("pickGameFile", { name: g.name ?? g.tid }),
        filters: [{ name: t("switchGames"), extensions: ["nsp", "xci"] }] });
      if (typeof f !== "string") { error = ""; return; }
      await run(() => api.setGameFile(g.tid, f));
      if (error) return;
      await run(() => api.launchGame(g.tid));
    }
    if (!error) notice = t("launched", { name: g.name ?? g.tid });
  }

  async function install(m: ModEntry, only?: string) {
    if ("nsfw" in m && m.nsfw) {
      const ok = await confirm(t("confirmNsfwInstall"), {
        title: t("confirmNsfwTitle"),
        kind: "warning",
        okLabel: t("install"),
        cancelLabel: t("cancel"),
      });
      if (!ok) return;
    }
    installingId = m.id;
    failedId = null;
    await stage(() => api.prepareInstall(selected!.tid, m.id), only);
  }

  const gbDetails = new Map<string, GbDetail>();
  async function openGb(m: GbMod) {
    gbOpen = m;
    gbDetail = gbDetails.get(m.id) ?? null;
    gbDetailErr = "";
    openModal(gbDlg);
    if (gbDetail) return;
    try {
      const d = await api.gamebananaDetail(Number(m.files[0].src));
      gbDetails.set(m.id, d);
      if (gbOpen?.id === m.id) gbDetail = d;
    } catch (e) {
      if (gbOpen?.id === m.id) gbDetailErr = trErr(String(e));
    }
  }

  // descrição do GameBanana vem em HTML: só texto, sem renderizar markup de terceiros
  const plain = (html: string) =>
    (new DOMParser().parseFromString(html.replace(/<br\s*\/?>|<\/p>|<\/li>/gi, "\n"), "text/html").body.textContent ?? "")
      .replace(/\n{3,}/g, "\n\n")
      .trim();

  async function installLocal() {
    if (!selected || busy) return;
    const f = await open({ multiple: false, filters: [{ name: t("modArchive"), extensions: ["zip", "7z", "rar"] }] });
    if (typeof f === "string") await stage(() => api.prepareLocal(selected!.tid, f));
  }

  async function stage(prep: () => Promise<Prepared>, only?: string) {
    if (!selected || busy) return;
    busy = true;
    notice = "";
    progress = { received: 0, total: null };
    const p = await run(prep);
    progress = null;
    if (!p) {
      failedId = installingId;
      busy = false;
      return;
    }
    if (p.isSave) {
      savePrepared = p;
      openModal(saveDlg);
      return;
    }
    if (only !== undefined) {
      await commit(p.token, [only]);
      return;
    }
    if (p.roots.length === 1) {
      await commit(p.token, [p.roots[0].key]);
    } else {
      prepared = p;
      checked = {};
    }
  }

  async function commit(token: string, keys: string[]) {
    const r = await run(() => api.commitInstall(token, keys));
    progress = null;
    prepared = null;
    busy = false;
    if (r) {
      const isSave = r.some((i) => i.destination === "save");
      notice = isSave ? t("saveInstalledNotice") : t(emu === "ryujinx" ? "installedNoticeRyu" : "installedNotice");
    }
    else failedId = installingId;
    await refreshInstalled();
  }

  async function cancel() {
    if (prepared) await run(() => api.cancelInstall(prepared!.token));
    prepared = null;
    busy = false;
  }

  async function cancelSave() {
    if (savePrepared) await run(() => api.cancelInstall(savePrepared!.token));
    savePrepared = null;
    saveDlg?.close();
    busy = false;
  }

  async function confirmSave() {
    if (!savePrepared) return;
    const token = savePrepared.token;
    const key = savePrepared.roots[0]?.key;
    saveDlg?.close();
    if (key) await commit(token, [key]);
    savePrepared = null;
  }

  function remove(i: InstalledView) {
    removing = i;
    openModal(removeDlg);
  }

  function cancelRemove() {
    removing = null;
    removeDlg?.close();
  }

  async function confirmRemove() {
    if (!removing) return;
    const item = removing;
    removing = null;
    removeDlg?.close();
    await run(() => api.uninstall(item.tid, item.folder));
    await refreshInstalled();
  }

  onMount(() => {
    document.documentElement.lang = locale();
    if (!localStorage.getItem("guideSeen")) openModal(guide);
    // Mica só existe no Windows 11 (platformVersion >= 13 no Chromium/WebView2); fora dele o fundo fica opaco
    const uad = (navigator as Navigator & {
      userAgentData?: { platform: string; getHighEntropyValues(h: string[]): Promise<{ platformVersion?: string }> };
    }).userAgentData;
    uad?.getHighEntropyValues(["platformVersion"]).then((v) => {
      micaOk = uad.platform === "Windows" && parseInt(v.platformVersion ?? "0") >= 13;
      applyMica();
    });
    const un = listen<{ received: number; total: number | null }>("download-progress", (e) => {
      progress = e.payload;
    });
    const unNsz = listen<{ percent: number | null; step: string }>("nsz-progress", (e) => {
      nszStep = e.payload;
    });
    const unStage = listen<string>("nsz-stage", (e) => {
      if (e.payload === "fallback") nszLog = "";
      if (nszCur) nszCur.stage = e.payload;
    });
    const unLog = listen<string>("nsz-log", (e) => {
      nszLog = (nszLog + e.payload + "\n").slice(-6000);
    });
    const unGbMore = listen<GbMore>("gamebanana-more", (e) => {
      if (
        e.payload.requestId === gbSeq &&
        e.payload.tid === selected?.tid &&
        gb &&
        gb.tid === selected?.tid &&
        gbAll === e.payload.all
      ) {
        if (gb.status === "loading") gbPendingMore.set(e.payload.requestId, e.payload);
        gb = { ...gb, mods: e.payload.mods };
      }
    });
    reloadEmu();
    if (autoUpdate) api.checkUpdate().then((r) => { if (r.version && !guide?.open) { update = r; openModal(upd); } }).catch(() => {});
    return () => {
      un.then((f) => f());
      unNsz.then((f) => f());
      unStage.then((f) => f());
      unLog.then((f) => f());
      unGbMore.then((f) => f());
    };
  });
</script>

<svelte:window onfocus={refreshFw} />

{#snippet modRow(m: ModEntry | GbMod)}
  {@const isInstalled = installed.some((i) => i.modId === m.id)}
  <div class="row">
    {#if "likes" in m && m.thumb}<img class="thumb" src={m.thumb} alt="" loading="lazy" />{/if}
    <div class="info">
      <div class="name" title={m.name}>{m.name}</div>
      <div class="sub" title={modPath(m)}>{#if m.source !== "official" && m.source !== "gamebanana"}<span class="src">{SOURCE_LABEL[m.source]}</span> {/if}{"likes" in m ? `${m.featured ? "★ " : ""}${m.likes} ♥` : modPath(m)}</div>
    </div>
    {#if m.version}
      {@const match = selected?.version === m.version}
      <span class="badge" class:ok={match} title={match ? t("versionMatch") : undefined}>{#if match}{@render icon(ICON.check)}{/if}{m.version}</span>
    {/if}
    <span class="size">{m.size ? fmtSize(m.size) : ""}</span>
    <button disabled={busy || isInstalled} onclick={() => install(m)}>{isInstalled ? t("gbInstalled") : t("install")}</button>
    <button class="ghost" onclick={() => openUrl(githubUrl(m))}>{m.source === "gamebanana" ? t("viewGb") : t("viewGithub")}</button>
  </div>
  {#if m.kind === "archive"}
    {#if (contents[m.id]?.length ?? 0) > 1}
      <div class="subrows">
        {#each contents[m.id] as r (r.key)}
          <div class="row sm">
            <div class="info"><div class="name" title={r.name}>{r.name}</div></div>
            <span class="size">{t("nFiles", { n: r.fileCount })}</span>
            <button disabled={busy} onclick={() => install(m, r.key)}>{t("install")}</button>
          </div>
        {/each}
      </div>
    {/if}
  {/if}
{/snippet}

{#snippet gbCard(m: GbMod)}
  {@const isInstalled = installed.some((i) => i.modId === m.id)}
  {@const working = busy && installingId === m.id}
  {@const failed = !busy && failedId === m.id}
  <div class="card" class:working class:failed>
    <button class="card-main" title={m.name} onclick={() => openGb(m)}>
      <span class="card-img">
        {#if m.thumb}<img src={m.thumb} alt="" loading="lazy" decoding="async" />{/if}
        {#if failed}<span class="card-flag bad">{@render icon(ICON.alert)}{t("gbFailed")}</span>
        {:else if isInstalled}<span class="card-flag">{@render icon(ICON.check)}{t("gbInstalled")}</span>
        {:else if m.nsfw}<span class="card-flag nsfw">18+</span>{/if}
        {#if working}<span class="card-bar"><span class="card-fill" class:indet={!progress?.total} style:width={progress?.total ? `${Math.min(100, (progress.received / progress.total) * 100)}%` : undefined}></span></span>{/if}
      </span>
      <span class="card-name">{m.featured ? "★ " : ""}{m.name}</span>
      <span class="card-meta">
        {#if m.category}<span class="card-cat">{m.category}</span>{:else}<span></span>{/if}
        <span class="card-stats"><span title={t("gbViews")}>👁 {compact(m.views)}</span><span>{compact(m.likes)} ♥</span></span>
      </span>
    </button>
    <button class="card-btn" class:installed={isInstalled} disabled={busy || isInstalled} aria-busy={working} onclick={() => install(m)}>{#if working}{@render spinner()}{:else if isInstalled}{@render icon(ICON.check)}{/if}{working ? t("gbInstalling") : failed ? t("retry") : isInstalled ? t("gbInstalled") : t("install")}</button>
  </div>
{/snippet}

{#snippet spinner()}
  <span class="spinner" aria-hidden="true"></span>
{/snippet}

{#snippet icon(d: string)}
  <svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path {d} /></svg>
{/snippet}

<div class="app">
  <nav class="rail" aria-label="Eden Mod Manager">
    <button class="logo-btn" popovertarget="app-menu" aria-haspopup="menu" aria-label={t("menu")} title={t("menu")}><img class="logo" src="/logo.png" alt="" /></button>
    <div id="app-menu" class="menu" popover role="menu">
      <button role="menuitem" popovertarget="app-menu" popovertargetaction="hide" onclick={openSettings}>{@render icon(ICON.settings)}{t("settings")}</button>
      <button role="menuitem" popovertarget="app-menu" popovertargetaction="hide" onclick={() => openModal(guide)}>{@render icon(ICON.help)}{t("guideOpen")}</button>
      <button role="menuitem" popovertarget="app-menu" popovertargetaction="hide" disabled={busy} onclick={checkUpdate}>{@render icon(ICON.update)}{t("updCheck")}</button>
    </div>
    <button class="rail-btn" title={t("tabMods")} aria-label={t("tabMods")} aria-current={tab === "mods" ? "page" : undefined} onclick={() => (tab = "mods")}>{@render icon(ICON.mods)}</button>
    <button class="rail-btn" title={t("tabNsz")} aria-label={t("tabNsz")} aria-current={tab === "nsz" ? "page" : undefined} disabled={!emuDir} onclick={() => showNsz()}>{@render icon(ICON.nsz)}</button>
    <span class="spacer"></span>
    <button class="rail-btn" class:spin={loadingCatalog} title={loadingCatalog ? t("refreshing") : t("refreshCatalog")} aria-label={t("refreshCatalog")} disabled={busy || !emuDir} aria-busy={loadingCatalog} onclick={() => loadCatalog(true)}>{@render icon(ICON.refresh)}</button>
  </nav>
  <div class="content">
    <header class="topbar">
      <h1>{tab === "nsz" ? t("tabNsz") : t("tabMods")}</h1>
      <div class="seg" role="group" aria-label={t("emulator")}>
        {#each Object.entries(EMU_NAME) as [k, name] (k)}
          <button aria-pressed={emu === k} disabled={busy} onclick={() => emu !== k && switchEmu(k as Emu)}>{name}</button>
        {/each}
      </div>
      <span class="spacer"></span>
      {#if catalog}
        <span class="muted small">{t("catalogFrom", { date: new Date(catalog.fetchedAt * 1000).toLocaleDateString(locale()) })}</span>
      {/if}
      <span class="path muted small" title={emuDir ?? ""}>{t("edenLabel", { path: emuDir ?? t("notFound") })}</span>
      <button class="link" onclick={() => changeDir()}>{t("change")}</button>
    </header>

    {#if !emuDir}
      <div class="empty panel">
        <p>{t("noEden")}</p>
        <p class="muted">{t("noEdenHint", { path: EMU_HINT[emu] })}</p>
        <button class="primary" onclick={() => changeDir()}>{t("pickEden")}</button>
      </div>
    {:else if tab === "nsz"}
      <section class="nsz">
        <div class="toolbar">
          <label class="search">
            {@render icon(ICON.search)}
            <input type="search" placeholder={t("filterRoms")} aria-label={t("filterRoms")} bind:value={romFilter} />
          </label>
          <span class="spacer"></span>
          <button class="primary" disabled={busy} onclick={pickRom}>{@render icon(ICON.plus)}{t("pickFile")}</button>
        </div>
        <div class="panel nsz-head">
          <details class="what"><summary><span class="chev">{@render icon(ICON.chev)}</span>{t("nszWhat")}</summary><p class="muted">{emu === "eden" ? t("nszIntro") : t("nszIntroOther")}</p></details>
          <label class="del"><input type="checkbox" bind:checked={deleteSource} /> {t("deleteOriginal")}
            {#if deleteSource}<span class="muted"> — {t("quickVerifyHint")}</span>{/if}</label>
          {#if romFilter}<p class="muted small">{t("romCount", { shown: shownRoms.length, total: romList.length })}</p>{/if}
          {#if nszCur}
            <div
              class="running"
              role="progressbar"
              aria-label={t("nszProgress")}
              aria-valuemin="0"
              aria-valuemax="100"
              aria-valuenow={nszStep?.percent ?? undefined}
              aria-valuetext={`${runVerb(nszCur.op)} ${cleanName(nszCur.name)}`}
            >
              <div class="run-top">
                <strong>{runVerb(nszCur.op)}</strong>
                <span class="run-name">{cleanName(nszCur.name)}</span>
                <span class="muted">{stageText}</span>
                <span class="spacer"></span>
                <span class="muted">{elapsed}</span>
              </div>
              <div class="track">
                <div class="fill" class:indet={nszStep?.percent == null} style:width={nszStep?.percent == null ? undefined : `${nszStep.percent}%`}></div>
              </div>
            </div>
          {/if}
          {#if nszLog}<pre class="log" use:stick={nszLog}>{nszLog}</pre>{/if}
        </div>
        <div class="nsz-list">
          {#each groups as g (g.key)}
            <details class="panel group" open={!!romFilter}>
              <summary class="panel-head">
                <span class="chev">{@render icon(ICON.chev)}</span>
                <span class="avatar" aria-hidden="true">{#if romCover(g.key)}<img src={romCover(g.key)} alt="" />{:else}{initials(g.name)}{/if}</span>
                <span class="gname">{g.name}</span>
                <span class="count">{g.items.length}</span>
                <span class="muted small">{fmtSize(g.size)}</span>
              </summary>
              {#each g.items as r (r.path)}
                <div class="row" title={r.path}>
                  <div class="info">
                    <div class="name">
                      {#if kindOf(r.name)}<span class="badge tag">{t(kindOf(r.name)!)}</span>{/if}{itemName(r, g.name)}
                    </div>
                    {#if nszCur?.path === r.path}
                      <div class="sub run">{@render icon(ICON.refresh)}{runVerb(nszCur.op)}… {elapsed}</div>
                    {:else if nszRes[r.path]}
                      <div class="sub" class:bad={!nszRes[r.path].ok}>{@render icon(nszRes[r.path].ok ? ICON.check : ICON.x)}{nszRes[r.path].text}</div>
                    {:else}
                      <div class="sub">{r.ext.toUpperCase()}{r.size ? ` · ${fmtSize(r.size)}` : ""}</div>
                    {/if}
                  </div>
                  {#if canVerify}<button class="quiet" disabled={busy} onclick={() => nsz("verify", r)}>{t("verify")}</button>{/if}
                  <button class="quiet" disabled={busy} onclick={() => nsz("info", r)}>{t("info")}</button>
                  {#if r.ext === "nsp" || r.ext === "xci"}
                    <button class="act" disabled={busy} onclick={() => nsz("compress", r)}>{t("compress")}</button>
                  {:else if r.ext === "nsz" || r.ext === "xcz"}
                    <button class="act" disabled={busy} onclick={() => nsz("decompress", r)}>{t("decompress")}</button>
                  {/if}
                </div>
              {/each}
            </details>
          {:else}
            <div class="hint">{@render icon(ICON.nsz)}<p>{t("noRoms")}</p></div>
          {/each}
        </div>
      </section>
    {:else}
      <main>
        <aside class="panel games">
          <h3 class="panel-head">{t("games")} <span class="count">{filteredGames.length}</span></h3>
          <label class="search">
            {@render icon(ICON.search)}
            <input type="search" placeholder={t("filterGames")} aria-label={t("filterGames")} bind:value={gameFilter} />
          </label>
          <div class="list">
            {#each filteredGames as g (g.tid)}
              <button
                class="game"
                class:sel={selected?.tid === g.tid}
                aria-current={selected?.tid === g.tid ? "true" : undefined}
                onclick={() => select(g)}
              >
                <span class="avatar" aria-hidden="true">{#if g.icon}<img src={g.icon} alt="" />{:else}{initials(g.name ?? g.tid)}{/if}</span>
                <span class="info">
                  <span class="name" title={g.name ?? g.tid}>{g.name ?? g.tid}</span>
                  <span class="sub"><span title={t("tidTitle")}>{g.tid}</span>{#if g.isCompressed}<span class="badge warn" title={t("nszCompressedTitle")}>NSZ</span>{/if}{#if g.version}<span class="badge">{g.version}</span>{/if}</span>
                </span>
              </button>
            {:else}
              <p class="muted pad">{t("noGames")}</p>
            {/each}
          </div>
        </aside>

        <div class="pane">
        {#if selected}
          <div class="filters">
            <div class="seg tabs" role="group" aria-label={t("viewLabel")}>
              <button aria-pressed={view === "repo"} onclick={() => (view = "repo")}>{t("viewRepo")} <span class="count">{available.length}</span></button>
              <button aria-pressed={view === "gb"} onclick={() => (view = "gb")}>GameBanana <span class="count" title={gb?.status === "error" ? gb.error : undefined}>{#if !gb || gb.tid !== selected.tid || gb.status === "loading"}{@render spinner()}{:else if gb.status === "ok"}{gb.mods.length}{:else if gb.status === "notfound"}0{:else}!{/if}</span></button>
            </div>
            <label class="search">
              {@render icon(ICON.search)}
              <input type="search" placeholder={t("filterMods")} aria-label={t("filterMods")} bind:value={search} />
            </label>
            {#if view === "repo"}
              <div class="seg" role="group" aria-label={t("srcAll")}>
                {#each ["all", "official", "theboy181", "wiki", "ptbr"] as const as s (s)}
                  <button aria-pressed={srcFilter === s} onclick={() => (srcFilter = s)}>{s === "all" ? t("srcAll") : s === "official" ? t("srcOfficial") : SOURCE_LABEL[s]}</button>
                {/each}
              </div>
              {#if selected.version}
                <div class="seg" role="group" aria-label={t("onlyMatchTip", { v: selected.version })}>
                  <button aria-pressed={onlyMatch} title={t("onlyMatchTip", { v: selected.version })} onclick={() => (onlyMatch = !onlyMatch)}>{#if onlyMatch}{@render icon(ICON.check)}{/if}{t("onlyMatch", { v: selected.version })}</button>
                </div>
              {/if}
            {:else if view === "gb"}
              <div class="seg" role="group" aria-label={t("gbMode")}>
                <button aria-pressed={gbSort === "likes"} onclick={() => (gbSort = "likes")}>{t("gbSortLikes")}</button>
                <button aria-pressed={gbSort === "newest"} onclick={() => (gbSort = "newest")}>{t("gbSortNewest")}</button>
                <button aria-pressed={gbSort === "views"} onclick={() => (gbSort = "views")}>{t("gbSortViews")}</button>
                <button aria-pressed={gbSort === "downloads"} onclick={sortByDownloads}>{t("gbSortDownloads")}</button>
                <button aria-pressed={gbSort === "name"} onclick={() => (gbSort = "name")}>{t("gbSortName")}</button>
              </div>
              <div class="seg" role="group" aria-label={t("gbFeaturedOnly")}>
                <button aria-pressed={gbFeaturedOnly} title={t("gbFeaturedOnlyTip")} onclick={() => (gbFeaturedOnly = !gbFeaturedOnly)}>{#if gbFeaturedOnly}{@render icon(ICON.check)}{/if}★ {t("gbFeaturedOnly")}</button>
              </div>
              <div class="seg" role="group" aria-label={t("filterNsfw")}>
                <button aria-pressed={hideNsfw} title={t("filterNsfwTip")} onclick={toggleNsfw}>{#if hideNsfw}{@render icon(ICON.check)}{/if}{t("filterNsfw")}</button>
              </div>
            {/if}
          </div>
          {#if smash || fw?.needed}
            <div class="warn dependency-notice" class:ok={fw && !fwMissing} role="note">
              {@render icon(fw && !fwMissing ? ICON.check : ICON.alert)}
              <p>{fw ? (fwMissing ? t(smash ? "dependenciesMissing" : "skylineMissing", { list: fwMissing }) : t(smash ? "dependenciesReady" : "skylineReady")) : t("dependenciesNotice")}</p>
              {#if smash}<button onclick={() => { void refreshFw(); openModal(dependencies); }}>{@render icon(ICON.help)}{t("dependencies")}</button>{/if}
              {#if fw && fwMissing}<button disabled={busy} onclick={installFrameworks}>{t(smash ? "dependenciesInstallNow" : "skylineInstallNow")}</button>{/if}
            </div>
          {/if}
        {/if}
        <div class="detail">
          {#if !selected}
            <div class="hint">{@render icon(ICON.mods)}<p>{t("selectGame")}</p></div>
          {:else}
            <div class="head">
              {#if selected.icon}<img class="cover" src={selected.icon} alt="" />{/if}
              <div class="head-title">
                <h2>{selected.name ?? selected.tid}</h2>
                {#if selected.updateFile && selected.updateRegistered}
                  <span class="sub-update">{@render icon(ICON.check)}{t("updatePackageFound")}</span>
                {:else if selected.updateFile}
                  <span class="sub-update warn" title={t("updatePackageUnregisteredTip")}>{@render icon(ICON.alert)}{t("updatePackageUnregistered")}</span>
                {/if}
              </div>
              <span class="spacer"></span>
              <button class="primary" disabled={busy} onclick={launchGame}>{@render icon(ICON.play)}{t("play")}</button>
            </div>

            {#if selected.isCompressed}
              <div class="warn nsz-alert" role="note">
                {@render icon(ICON.nsz)}
                <div class="nsz-alert-text">
                  <p><strong>{t("nszCompressedTitle")}</strong> — {t("nszCompressedDesc")}</p>
                </div>
                <button onclick={() => showNsz(selected?.name ?? selected?.tid)}>{@render icon(ICON.nsz)}{t("decompressInNsz")}</button>
              </div>
            {/if}

            <section class="panel">
              <h3 class="panel-head">{t("installed")} <span class="count">{installed.length}</span><span class="spacer"></span><button class="icon-btn" disabled={busy} aria-label={t("addLocal")} title={t("addLocal")} onclick={installLocal}>{@render icon(ICON.plus)}</button><button class="icon-btn" aria-label={t("openModFolder")} title={t("openModFolder")} onclick={() => api.openModFolder(selected!.tid)}>{@render icon(ICON.folder)}</button></h3>
              {#each installed as i (i.folder)}
                {@const up = newer(i)}
                <div class="row" class:off={!i.enabled}>
                  <div class="info">
                    <div class="name" title={i.folder}>{i.folder}</div>
                    <div class="sub" title={i.modId}>{i.modId}</div>
                  </div>
                  {#if up}<span class="badge" title={t("updateAvailable", { v: up.version ?? "" })}>{i.version} → {up.version}</span>
                    <button disabled={busy} onclick={() => install(up, i.rootKey)}>{t("updateMod")}</button>{/if}
                  {#if i.destination === "save"}
                    <span class="badge">{t("saveBadge")}</span>
                  {:else}
                    <button class="ghost" title={t("toggleHint")} onclick={() => toggle(i)}>{i.enabled ? t("disable") : t("enable")}</button>
                  {/if}
                  <button class="danger" onclick={() => remove(i)}>{t("remove")}</button>
                </div>
              {:else}
                <p class="muted">{t("noInstalled")}</p>
              {/each}
              {#each conflicts as c (c.folders.join("|"))}
                <div class="row conflict" title={c.sample}>
                  <div class="info">
                    <div class="name">{t("conflict", { folders: c.folders.join(" × "), n: c.count })}</div>
                    <div class="sub">{c.sample}</div>
                  </div>
                </div>
              {/each}
            </section>

            {#if view === "repo"}
              <section class="panel">
                <h3 class="panel-head">{t("available")} <span class="count">{available.length}</span></h3>
                {#each available as m (m.id)}{@render modRow(m)}{:else}
                  <p class="muted">{q || srcFilter !== "all" || onlyMatch ? t("nothingMatches") : t("noAvailable")}</p>
                {/each}
              </section>
            {/if}

            {#if possible.length && view === "repo"}
              <section class="panel">
                <h3 class="panel-head">{t("possible")} <span class="count">{possible.length}</span></h3>
                {#each possible as m (m.id)}{@render modRow(m)}{/each}
              </section>
            {/if}

            {#if view === "gb"}
              <section class="panel">
                <h3 class="panel-head">GameBanana {#if gb?.tid === selected.tid && gb.status === "ok"}<span class="count">{gbList.length}</span>{/if}<span class="spacer"></span>
                  <span class="seg" role="group" aria-label={t("gbMode")}>
                    <button aria-pressed={!gbAll} onclick={() => setGbAll(false)}>{t("gbCurated")}</button>
                    <button aria-pressed={gbAll} onclick={() => setGbAll(true)}>{t("gbAll")}</button>
                  </span>
                </h3>
                <div class="warn" role="note">{@render icon(ICON.alert)}<p><strong>{t("gbWarnTitle")}</strong> {t("gbWarn")}</p></div>
                {#if gbCategories.length > 1 || gbCategory !== "all"}
                  <div class="cats-row">
                    <div class="seg" role="group" aria-label={t("gbCategories")}>
                      <button
                        aria-pressed={gbCategory === "all"}
                        onclick={() => (gbCategory = "all")}
                      >
                        {t("catAll")} <span class="count">{gbCategoryCounts.all ?? 0}</span>
                      </button>
                      {#each gbCategories as cat (cat)}
                        <button
                          aria-pressed={gbCategory === cat}
                          onclick={() => (gbCategory = cat)}
                        >
                          {cat} <span class="count">{gbCategoryCounts[cat] ?? 0}</span>
                        </button>
                      {/each}
                    </div>
                  </div>
                {/if}
                {#if !gb || gb.tid !== selected.tid || gb.status === "loading"}
                  <p class="muted loading">{@render spinner()}{gbAll ? t("gbLoadingAll") : t("gbLoading")}</p>
                  <div class="cards" aria-hidden="true">{#each { length: 12 } as _, i (i)}<div class="card skel"></div>{/each}</div>
                {:else if gb.status === "error"}
                  <p class="muted">{gb.error} <button class="link" onclick={() => loadGb(selected!)}>{t("retry")}</button></p>
                {:else if gb.status === "notfound"}
                  <p class="muted">{t("gbNotFound")} <button class="link" onclick={() => openUrl(`https://gamebanana.com/search?_sSearchString=${encodeURIComponent(selected!.name ?? "")}`)}>{t("gbSearchSite")}</button></p>
                {:else}
                  <div class="cards">
                    {#each gbList.slice(0, gbShown) as m (m.id)}{@render gbCard(m)}{/each}
                  </div>
                  {#if !gbList.length}
                    <p class="muted">{q || gbCategory !== "all" ? t("nothingMatches") : gbAll ? t("gbEmptyAll") : t("gbEmptyCurated")}</p>
                  {/if}
                  {#if gbList.length > gbShown}
                    <button class="ghost more" onclick={() => (gbShown += 50)}>{t("showMore", { n: gbList.length - gbShown })}</button>
                  {/if}
                {/if}
              </section>
            {/if}

            {#if view === "repo" && q && searchHits.length}
              <section class="panel">
                <h3 class="panel-head">{t("searchAll")} <span class="count">{searchHits.length}</span></h3>
                {#each searchResults as m (m.id)}{@render modRow(m)}{/each}
                {#if searchHits.length > 200}
                  <p class="muted">{t("showing", { shown: 200, total: searchHits.length })}</p>
                {/if}
              </section>
            {/if}
          {/if}
        </div>
        </div>
      </main>
    {/if}
  </div>

  <div class="toasts">
    {#if progress && busy}
      <div class="toast dl" role="progressbar" aria-label={t("downloadLabel")} aria-valuemin="0" aria-valuemax="100"
           aria-valuenow={progress.total ? Math.round((progress.received / progress.total) * 100) : undefined}>
        <div class="run-top small">
          <span>{t("downloadLabel")}</span><span class="spacer"></span>
          <span class="muted">{fmtSize(progress.received)}{progress.total ? ` / ${fmtSize(progress.total)}` : ""}</span>
        </div>
        <div class="track"><div class="fill" class:indet={!progress.total} style:width={progress.total ? `${Math.min(100, (progress.received / progress.total) * 100)}%` : undefined}></div></div>
      </div>
    {/if}
    <div role="alert">{#if error}<div class="toast err"><span class="ico">{@render icon(ICON.alert)}</span>{error}<button class="x" aria-label={t("dismiss")} onclick={() => (error = "")}>{@render icon(ICON.x)}</button></div>{/if}</div>
    <div role="status">{#if notice}<div class="toast ok"><span class="ico">{@render icon(ICON.check)}</span>{notice}<button class="x" aria-label={t("dismiss")} onclick={() => (notice = "")}>{@render icon(ICON.x)}</button></div>{/if}</div>
  </div>


  <dialog bind:this={dlg} class="modal" aria-labelledby="dlg-title" tabindex="-1" oncancel={(e) => { e.preventDefault(); cancel(); }}>
    {#if prepared}
      <h3 id="dlg-title">{t("chooseTitle")}</h3>
      <p class="muted">{t("chooseHint")}</p>
      <div class="rootlist">
        {#each prepared.roots as r (r.key)}
          <label><input type="checkbox" bind:checked={checked[r.key]} /> {r.name}
            <span class="muted">{t("filesCount", { n: r.fileCount })}</span></label>
        {/each}
      </div>
      <div class="actions">
        <button class="ghost" onclick={cancel}>{t("cancel")}</button>
        <button
          class="primary"
          disabled={!prepared.roots.some((r) => checked[r.key])}
          onclick={() => commit(prepared!.token, prepared!.roots.filter((r) => checked[r.key]).map((r) => r.key))}
        >{t("installSelected")}</button>
      </div>
    {/if}
  </dialog>

  <dialog bind:this={saveDlg} class="modal" aria-labelledby="save-dlg-title" tabindex="-1" oncancel={(e) => { e.preventDefault(); cancelSave(); }}>
    {#if savePrepared}
      <h3 id="save-dlg-title">{t("saveTitle")}</h3>
      <p>{t("saveWarning")}</p>
      <p class="muted"><strong>{t("saveEmuClosedNotice")}</strong></p>
      <div class="actions">
        <button class="ghost" onclick={cancelSave}>{t("cancel")}</button>
        <button class="primary" onclick={confirmSave}>{t("saveConfirmBtn")}</button>
      </div>
    {/if}
  </dialog>

  <dialog bind:this={removeDlg} class="modal rm-modal" aria-labelledby="rm-title" tabindex="-1" oncancel={(e) => { e.preventDefault(); cancelRemove(); }}>
    {#if removing}
      <div class="rm-header">
        <div class="rm-badge">
          {@render icon(ICON.trash)}
        </div>
        <div class="rm-title-wrap">
          <h3 id="rm-title">{t("confirmRemoveTitle")}</h3>
          <p class="rm-sub">{removing.name || removing.folder}</p>
        </div>
      </div>
      <p class="rm-body">
        {removing.destination === "save" ? t("confirmRemoveSaveDesc") : t("confirmRemoveDesc")}
      </p>
      <div class="actions">
        <button class="ghost" onclick={cancelRemove}>{t("cancel")}</button>
        <button class="danger" onclick={confirmRemove}>{@render icon(ICON.trash)}{t("remove")}</button>
      </div>
    {/if}
  </dialog>

  <dialog bind:this={gbDlg} class="modal gbmod" aria-labelledby="gbm-title" tabindex="-1" onclose={() => (gbOpen = null)}>
    {#if gbOpen}
      {@const m = gbOpen}
      {@const img = gbDetail?.image ?? m.thumb}
      {@const isInstalled = installed.some((i) => i.modId === m.id)}
      <div class="gbm-img">{#if img}<img src={img} alt="" />{/if}</div>
      <h3 id="gbm-title">{m.name}</h3>
      <div class="gbm-stats">
        {#if m.category}<span class="card-cat">{m.category}</span>{/if}
        {#if gbDetail?.submitter}<span>{t("gbBy", { name: gbDetail.submitter })}</span>{/if}
        {#if gbDetail?.version}<span>v{gbDetail.version}</span>{/if}
        <span title={t("gbViews")}>👁 {compact(m.views)}</span>
        <span>{compact(m.likes)} ♥</span>
        {#if gbDetail}
          <span>⬇ {compact(gbDetail.downloads)}</span>
          {#if gbDetail.size}<span>{fmtSize(gbDetail.size)}</span>{/if}
          {#if gbDetail.updated}<span>{t("gbUpdated", { date: new Date(gbDetail.updated * 1000).toLocaleDateString(locale()) })}</span>{/if}
        {/if}
      </div>
      <div class="gbm-text">
        {#if gbDetailErr}
          <p class="muted">{gbDetailErr} <button class="link" onclick={() => openGb(m)}>{t("retry")}</button></p>
        {:else if !gbDetail}
          <p class="muted loading">{@render spinner()}{t("gbDetailLoading")}</p>
        {:else}
          <p>{plain(gbDetail.text) || t("gbNoText")}</p>
        {/if}
      </div>
      {#if m.nsfw}<p class="gbm-warn nsfw">{@render icon(ICON.alert)}{t("nsfwModWarn")}</p>{/if}
      <p class="gbm-warn">{@render icon(ICON.alert)}{t("gbWarnShort")}</p>
      <div class="actions">
        <button class="ghost" onclick={() => openUrl(githubUrl(m))}>{t("viewGb")}</button>
        <span class="spacer"></span>
        <button class="ghost" onclick={() => gbDlg?.close()}>{t("setClose")}</button>
        <button class="primary" disabled={busy || isInstalled} onclick={() => { gbDlg?.close(); install(m); }}>{isInstalled ? t("gbInstalled") : t("install")}</button>
      </div>
    {/if}
  </dialog>

  <dialog bind:this={guide} class="modal guide" aria-labelledby="guide-title" tabindex="-1" onclose={() => localStorage.setItem("guideSeen", "1")}>
    <h3 id="guide-title">{t("guideTitle")}</h3>
    <ol>
      {#each [["g1t", "g1b"], ["g2t", "g2b"], ["g3t", "g3b"], ["g4t", "g4b"]] as const as [h, b] (h)}
        <li><b>{t(h)}</b><span class="muted">{t(b)}</span></li>
      {/each}
    </ol>
    <div class="actions"><button class="primary" onclick={() => guide?.close()}>{t("guideDone")}</button></div>
  </dialog>

  <dialog bind:this={dependencies} class="modal dependencies" aria-labelledby="dependencies-title" tabindex="-1">
    <h3 id="dependencies-title">{t("dependencies")}</h3>
    <div class="sbody">
      <p>{t("dependenciesIntro")}</p>
      {#if emu !== "eden"}
        <div class="warn" role="note">{@render icon(ICON.alert)}<p>{t(emu === "yuzu" ? "dependenciesYuzu" : "dependenciesRyujinx")}</p></div>
      {/if}
      <ol>
        <li>
          <b>{t("dependenciesStep1")}</b>
          <p>{t("dependenciesAuto")}</p>
          {#if emu !== "yuzu" && fw}
            <p class="fw-status" class:ok={!fwMissing}>{fwMissing ? t("dependenciesMissing", { list: fwMissing }) : t("dependenciesReady")}</p>
            {#if fwMissing}
              <div class="btns"><button class="primary" disabled={busy} onclick={installFrameworks}>{t("dependenciesInstallNow")}</button></div>
            {/if}
          {/if}
        </li>
        <li>
          <b>{t("dependenciesStep3")}</b>
          <p>{t("dependenciesVerify")}</p>
        </li>
        <li>
          <b>{t("dependenciesStep4")}</b>
          <p>{t("dependenciesInstall")}</p>
        </li>
      </ol>
      <p class="muted">{t("dependenciesOther")}</p>
    </div>
    <div class="actions"><button class="primary" onclick={() => dependencies?.close()}>{t("setClose")}</button></div>
  </dialog>

  <dialog bind:this={upd} class="modal" aria-labelledby="upd-title" tabindex="-1">
    {#if update?.version}
      <h3 id="upd-title">{t("updTitle", { v: update.version })}</h3>
      <p class="muted">{t("updBody", { cur: update.current })}</p>
      {#if update.notes}<pre class="upd-notes">{update.notes}</pre>{/if}
      <div class="actions"><button class="ghost" onclick={() => upd?.close()}>{t("updLater")}</button><button class="primary" onclick={installUpdate}>{t("updNow")}</button></div>
    {/if}
  </dialog>

  <dialog bind:this={sett} class="modal settings" aria-labelledby="set-title" tabindex="-1">
    <h3 id="set-title">{t("settings")}</h3>
    <div class="sbody">
      <section class="srow">
        <h4>{t("language")}</h4>
        <div class="sctl">
          <div class="seg" role="group" aria-label={t("language")}>
            {#each [["pt", "Português"], ["en", "English"]] as const as [l, name] (l)}
              <button aria-pressed={i18n.lang === l} onclick={() => setLang(l)}>{name}</button>
            {/each}
          </div>
        </div>
      </section>
      {#if micaOk}
        <section class="srow">
          <h4>{t("setAppearance")}</h4>
          <div class="sctl">
            <label><input type="checkbox" checked={micaOn} onchange={(e) => setMica(e.currentTarget.checked)} /> {t("setMica")}</label>
          </div>
        </section>
      {/if}
      <section class="srow">
        <h4>{t("setUpdates")}</h4>
        <div class="sctl">
          <p class="muted small">{t("setVersion", { v: appVersion })}</p>
          <label><input type="checkbox" checked={autoUpdate} onchange={(e) => setAutoUpdate(e.currentTarget.checked)} /> {t("setAutoCheck")}</label>
          <button disabled={busy} onclick={checkUpdate}>{t("updCheck")}</button>
        </div>
      </section>
      <section class="srow">
        <h4>GameBanana</h4>
        <div class="sctl">
          <label><input type="checkbox" checked={hideNsfw} onchange={toggleNsfw} /> {t("filterNsfw")}</label>
          <p class="muted small">{t("filterNsfwTip")}</p>
        </div>
      </section>
      <section class="srow">
        <h4>{t("setFolders")}</h4>
        <div class="sctl">
          <div class="dirs">
            {#each emuDirs as d (d.kind)}
              <div class="dirrow">
                <b>{EMU_NAME[d.kind]}</b>
                <span class="path muted small" title={d.dir ?? ""}>{d.dir ?? t("notFound")}</span>
                <button disabled={busy} onclick={() => changeDir(d.kind)}>{t("change")}</button>
              </div>
            {/each}
          </div>
        </div>
      </section>
      <section class="srow">
        <h4>{t("setCatalog")}</h4>
        <div class="sctl">
          {#if catalog}<p class="muted small">{t("catalogFrom", { date: new Date(catalog.fetchedAt * 1000).toLocaleDateString(locale()) })}</p>{/if}
          <p class="muted small">{t("setCache", { size: storage?.cacheBytes ? fmtSize(storage.cacheBytes) : "0 KB" })}</p>
          <div class="btns">
            <button disabled={busy || !emuDir} onclick={() => loadCatalog(true)}>{t("refreshCatalog")}</button>
            <button disabled={busy} onclick={clearCache}>{t("setClear")}</button>
          </div>
        </div>
      </section>
      <section class="srow">
        <h4>{t("setNsz")}</h4>
        <div class="sctl">
          <p class="muted small">{storage?.toolsBytes ? fmtSize(storage.toolsBytes) : t("setNszNone")}</p>
          <button disabled={busy || !!nszCur || !storage?.toolsBytes} onclick={removeTools}>{t("setNszRemove")}</button>
        </div>
      </section>
      <section class="srow">
        <h4>{t("setAbout")}</h4>
        <div class="sctl">
          <div class="btns">
            <button onclick={() => openUrl("https://github.com/pendiego/Eden-Mod-Manager")}>{t("setRepo")}</button>
            <button onclick={() => openUrl("https://github.com/pendiego/Eden-Mod-Manager/releases/latest")}>{t("setReleases")}</button>
          </div>
          <p class="muted small">{t("setLegal")}</p>
        </div>
      </section>
    </div>
    <div class="actions">
      {#if error}<p class="serr" role="alert">{error}</p>{:else if notice}<p class="muted small" role="status">{notice}</p>{/if}
      <button class="primary" onclick={() => sett?.close()}>{t("setClose")}</button>
    </div>
  </dialog>
</div>

<style>
  :global(:root) {
    color-scheme: light dark;
    --base-a: 1;
    --base: rgb(12 12 13 / var(--base-a));
    --fg: #f1f1f2; --fg-soft: #d0d0d3; --muted: #8e8f96;
    --panel: rgb(255 255 255 / 0.025); --card: rgb(255 255 255 / 0.03); --card-hover: rgb(255 255 255 / 0.05);
    --input: rgb(255 255 255 / 0.03); --border: rgb(255 255 255 / 0.08); --border-soft: rgb(255 255 255 / 0.05);
    --primary: #f1f1f2; --on-primary: #0c0c0d;
    --ghost: rgb(255 255 255 / 0.06); --ghost-hover: rgb(255 255 255 / 0.11); --ghost-fg: #f1f1f2;
    --accent: #f1f1f2; --focus: #9cc2ff;
    --danger: #ef6b63; --danger-bg: rgb(229 83 75 / 0.16); --ok: #4ac26b; --ok-bg: rgb(74 194 107 / 0.14);
    --sel: rgb(255 255 255 / 0.09); --badge: rgb(255 255 255 / 0.08); --badge-ok: rgb(74 194 107 / 0.22);
    --toast: rgb(24 25 29 / 0.92); --backdrop: rgb(0 0 0 / 0.45); --shadow: 0 12px 32px rgb(5 8 20 / 0.45); --scroll: rgb(255 255 255 / 0.16);
  }
  :global(:root[data-mica]) { --base-a: 0.5; }
  @media (prefers-color-scheme: light) {
    :global(:root) {
      --base: rgb(242 243 246 / var(--base-a));
      --fg: #15171c; --fg-soft: #2f333c; --muted: #565c69;
      --panel: rgb(255 255 255 / 0.55); --card: rgb(255 255 255 / 0.85); --card-hover: rgb(255 255 255 / 1);
      --input: rgb(255 255 255 / 0.9); --border: rgb(0 0 0 / 0.09); --border-soft: rgb(0 0 0 / 0.05);
      --primary: #15171c; --on-primary: #ffffff;
      --ghost: rgb(0 0 0 / 0.06); --ghost-hover: rgb(0 0 0 / 0.1); --ghost-fg: #15171c;
      --accent: #1a56db; --focus: #1a4fd6;
      --danger: #b3261e; --danger-bg: rgb(179 38 30 / 0.1); --ok: #1d7a3e; --ok-bg: rgb(29 122 62 / 0.1);
      --sel: rgb(0 0 0 / 0.07); --badge: rgb(0 0 0 / 0.07); --badge-ok: rgb(29 122 62 / 0.15);
      --toast: rgb(255 255 255 / 0.95); --backdrop: rgb(0 0 0 / 0.25); --shadow: 0 12px 32px rgb(40 60 110 / 0.14); --scroll: rgb(0 0 0 / 0.18);
    }
    :global(:root[data-mica]) { --base-a: 0.6; }
  }
  /* Windows → Efeitos de transparência desligado: volta ao fundo opaco */
  @media (prefers-reduced-transparency: reduce) { :global(:root[data-mica]) { --base-a: 0.94; } }

  :global(:focus-visible) { outline: 2px solid var(--focus); outline-offset: 2px; }
  :global(::-webkit-scrollbar) { width: 10px; height: 10px; }
  :global(::-webkit-scrollbar-thumb) { background: var(--scroll); border-radius: 5px; border: 3px solid transparent; background-clip: padding-box; }
  :global(::-webkit-scrollbar-button) { display: none; }
  :global(html), :global(body) { background: transparent; }
  :global(body) { margin: 0; font: 14px/1.45 "Geist Variable", "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif; letter-spacing: -0.006em; color: var(--fg); height: 100vh; overflow: hidden; }
  :global(#svelte) { height: 100%; }

  .app { display: flex; height: 100vh; background-color: var(--base); }
  .rail { display: flex; flex-direction: column; align-items: center; gap: 6px; width: 56px; flex-shrink: 0; padding: 12px 0; box-sizing: border-box; border-right: 1px solid var(--border); }
  .logo { width: 32px; height: 32px; display: block; }
  .rail-btn { width: 38px; height: 38px; justify-content: center; padding: 0; background: transparent; color: var(--muted); border-radius: 10px; }
  .rail-btn:hover:not(:disabled) { background: var(--ghost); color: var(--fg); }
  .rail-btn[aria-current="page"] { background: var(--sel); color: var(--fg); }
  .rail-btn.spin :global(.icon) { animation: rot 1s linear infinite; }
  @keyframes rot { to { transform: rotate(360deg); } }
  .spinner { display: inline-block; flex-shrink: 0; width: 12px; height: 12px; box-sizing: border-box; border: 2px solid currentColor; border-right-color: transparent; border-radius: 50%; animation: rot 0.8s linear infinite; vertical-align: -2px; }
  .card-btn .spinner { margin-right: 6px; }
  .loading { display: flex; align-items: center; gap: 8px; }
  .content { display: flex; flex-direction: column; flex: 1; min-width: 0; min-height: 0; }
  .topbar { display: flex; align-items: center; gap: 14px; height: 52px; padding: 0 20px; box-sizing: border-box; border-bottom: 1px solid var(--border); flex-shrink: 0; }
  h1 { margin: 0; font-size: 15px; font-weight: 600; letter-spacing: -0.01em; }
  .seg { display: flex; gap: 2px; padding: 2px; border-radius: 8px; background: var(--input); border: 1px solid var(--border); }
  .seg button { background: transparent; color: var(--muted); border-radius: 6px; padding: 3px 12px; font-size: 12px; font-weight: 500; }
  .seg button[aria-pressed="true"] { background: var(--sel); color: var(--fg); }
  .topbar .path { max-width: 34%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .link { background: none; color: var(--fg-soft); padding: 0; text-decoration: underline; text-underline-offset: 2px; font-size: 12px; }
  .spacer { flex: 1; }
  .muted { color: var(--muted); }
  .small { font-size: 12px; }
  .pad { padding: 12px; }
  .hint { margin: auto; display: flex; flex-direction: column; align-items: center; gap: 10px; color: var(--muted); }
  .hint p { margin: 0; }
  .hint :global(.icon) { width: 28px; height: 28px; opacity: 0.6; }
  .icon { width: 16px; height: 16px; flex-shrink: 0; }

  input:not([type="checkbox"]) { background: var(--input); color: inherit; border: 1px solid var(--border); border-radius: 8px; padding: 7px 10px; box-sizing: border-box; width: 100%; font: inherit; }
  .search { display: flex; align-items: center; gap: 8px; width: min(360px, 100%); height: 34px; padding: 0 10px; box-sizing: border-box; border-radius: 8px; background: var(--input); border: 1px solid var(--border); color: var(--muted); flex-shrink: 0; }
  .search:focus-within { outline: 2px solid var(--focus); outline-offset: 2px; }
  .search input { flex: 1; min-width: 0; height: 100%; border: 0; background: transparent; padding: 0; border-radius: 0; }
  .search input:focus-visible { outline: none; }

  button { display: inline-flex; align-items: center; gap: 6px; background: var(--ghost); color: var(--ghost-fg); border: 0; border-radius: 8px; padding: 6px 12px; cursor: pointer; font: inherit; white-space: nowrap; transition: background-color 0.15s, color 0.15s, transform 0.1s; }
  button:active:not(:disabled) { transform: translateY(1px); }
  input[type="checkbox"] { accent-color: var(--fg); width: 15px; height: 15px; margin: 0; flex-shrink: 0; cursor: pointer; }
  label:has(> input[type="checkbox"]) { display: flex; align-items: center; gap: 8px; cursor: pointer; }
  button:hover:not(:disabled) { background: var(--ghost-hover); }
  button:disabled { opacity: 0.4; cursor: not-allowed; }
  button.primary { background: var(--primary); color: var(--on-primary); font-weight: 600; padding: 7px 14px; }
  button.primary:hover:not(:disabled) { background: var(--primary); filter: brightness(0.9); }
  button.ghost { background: transparent; color: var(--muted); }
  button.ghost:hover:not(:disabled) { background: var(--ghost); color: var(--fg); }
  button.danger { background: transparent; color: var(--danger); }
  button.danger:hover:not(:disabled) { background: var(--danger-bg); }
  button.icon-btn { padding: 4px; background: transparent; color: var(--muted); }

  /* Secoes: caixa com borda fina, linhas divididas */
  .panel { background: var(--panel); border: 1px solid var(--border); border-radius: 12px; overflow: clip; }
  .panel > p.muted { margin: 0; padding: 14px 16px; }
  .panel-head { display: flex; align-items: center; gap: 8px; margin: 0; padding: 12px 16px; border-bottom: 1px solid var(--border); font-size: 13px; font-weight: 500; color: var(--muted); }
  .count { min-width: 18px; padding: 0 6px; border-radius: 9px; background: var(--badge); color: var(--fg-soft); font-size: 11px; font-weight: 600; line-height: 18px; text-align: center; font-variant-numeric: tabular-nums; }
  .row { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--border-soft); transition: background-color 0.15s; }
  .row:last-child { border-bottom: 0; }
  .row:hover { background: var(--ghost); }
  .row.sm { padding: 6px 16px 6px 32px; font-size: 13px; }
  .row.off .info { opacity: 0.5; }
  .row.conflict .name { color: var(--danger); }
  .subrows { background: var(--input); border-bottom: 1px solid var(--border-soft); }

  /* Sidebar de jogos + detalhe */
  main { display: grid; grid-template-columns: 300px 1fr; flex: 1; min-height: 0; }
  .games { display: flex; flex-direction: column; min-height: 0; border: 0; border-right: 1px solid var(--border); border-radius: 0; background: transparent; }
  .games .panel-head { padding: 14px 16px 10px; border: 0; color: var(--fg); font-size: 14px; font-weight: 600; }
  .games .search { width: auto; margin: 0 12px 10px; }
  .list { display: flex; flex-direction: column; gap: 1px; overflow-y: auto; min-height: 0; padding: 0 8px 12px; }
  .pane { display: flex; flex-direction: column; min-width: 0; min-height: 0; }
  .filters { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding: 12px 28px; border-bottom: 1px solid var(--border); flex-shrink: 0; }
  .filters .search { flex: 1 1 120px; min-width: 110px; width: auto; max-width: 240px; }
  .seg button :global(.icon) { width: 11px; height: 11px; vertical-align: -1px; margin-right: 4px; }
  .detail { display: flex; flex-direction: column; gap: 20px; overflow-y: auto; flex: 1; min-height: 0; padding: 22px 28px 32px; }
  .detail h2 { margin: 0; font-size: 22px; font-weight: 600; letter-spacing: -0.02em; line-height: 1.15; text-wrap: balance; }
  .detail .panel-head { color: var(--fg); }
  .game { display: flex; align-items: center; gap: 10px; width: 100%; text-align: left; background: transparent; color: var(--muted); border-radius: 8px; padding: 6px 8px; }
  .game:hover:not(:disabled) { background: var(--ghost); color: var(--fg); }
  .game.sel { background: var(--sel); color: var(--fg); }
  .avatar { width: 28px; height: 28px; border-radius: 6px; display: grid; place-items: center; flex-shrink: 0; background: var(--badge); color: var(--fg-soft); font-size: 11px; font-weight: 600; overflow: hidden; }
  .avatar img, img.cover { width: 100%; height: 100%; object-fit: cover; display: block; }
  .game .avatar, .group .avatar { width: 36px; height: 36px; }
  .head { display: flex; align-items: center; gap: 14px; }
  .head img.cover { width: 56px; height: 56px; border-radius: 10px; flex-shrink: 0; }
  .head-title { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
  .head-title h2 { margin: 0; }
  .sub-update { font-size: 12px; color: #34d399; display: flex; align-items: center; gap: 4px; }
  .sub-update :global(.icon) { width: 13px; height: 13px; }
  .sub-update.warn { color: #e09a1a; }
  .nsz-alert { margin: 14px 0 0; align-items: center; }
  .nsz-alert-text { flex: 1; min-width: 0; }
  .nsz-alert button { flex-shrink: 0; }
  .thumb { width: 48px; height: 30px; border-radius: 4px; object-fit: cover; flex-shrink: 0; background: var(--badge); }
  .more { display: block; margin: 8px auto 12px; }
  .cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(200px, 1fr)); gap: 14px; padding: 14px; }
  /* altura fixa: capa 16:9, nome sempre em 2 linhas (reticências se longo), meta e botão de uma linha */
  .card { position: relative; display: flex; flex-direction: column; background: var(--ghost); border-radius: 10px; overflow: hidden; transition: background-color 0.15s, transform 0.15s, box-shadow 0.15s; }
  .card:hover { background: var(--ghost-hover); transform: translateY(-2px); box-shadow: var(--shadow); }
  .card.working { outline: 2px solid var(--accent); }
  .card-main { display: flex; flex-direction: column; align-items: stretch; width: 100%; padding: 0; gap: 0; text-align: left; background: transparent; border-radius: 0; font-weight: normal; }
  .card-main:hover:not(:disabled) { background: transparent; }
  .card-main:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .card-img { position: relative; display: block; aspect-ratio: 16 / 9; overflow: hidden; background: var(--badge); }
  .card-img img { width: 100%; height: 100%; object-fit: cover; display: block; transition: transform 0.25s; }
  .card:hover .card-img img { transform: scale(1.04); }
  .card-flag { position: absolute; top: 6px; left: 6px; display: inline-flex; align-items: center; gap: 4px; padding: 3px 8px; border-radius: 6px; background: #15803d; color: #ffffff; border: 1px solid #22c55e; box-shadow: 0 2px 8px rgb(0 0 0 / 0.5); font-size: 11px; font-weight: 600; letter-spacing: 0.02em; }
  .card-flag :global(.icon) { width: 12px; height: 12px; }
  .card-bar { position: absolute; left: 0; right: 0; bottom: 0; height: 4px; background: rgb(0 0 0 / 0.4); }
  .card-fill { display: block; height: 100%; background: var(--accent); transition: width 0.2s; }
  .card-fill.indet { width: 100%; opacity: 0.6; animation: pulse 1s ease-in-out infinite; }
  .card-name { display: -webkit-box; -webkit-line-clamp: 2; line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; overflow-wrap: anywhere; white-space: normal; box-sizing: content-box; height: 2.6em; margin: 8px 10px 0; font-weight: 500; line-height: 1.3; }
  .card-meta { display: flex; justify-content: space-between; align-items: center; gap: 8px; margin: 4px 10px 8px; color: var(--muted); font-size: 12px; line-height: 18px; white-space: nowrap; font-variant-numeric: tabular-nums; }
  .card-cat { max-width: 95px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11px; color: var(--muted); background: var(--badge); padding: 0 5px; border-radius: 4px; font-weight: 500; }
  .card-stats { display: flex; gap: 8px; margin-left: auto; }
  .card-btn { margin: auto 10px 10px; }
  .card-btn.installed { opacity: 0.9; background: var(--ghost); color: var(--ok); border-color: var(--ok); cursor: default; }
  .card-btn.installed :global(.icon) { width: 12px; height: 12px; margin-right: 4px; vertical-align: -1px; }
  .card.skel { height: 238px; animation: pulse 1.2s ease-in-out infinite; pointer-events: none; }
  @keyframes pulse { 50% { opacity: 0.45; } }
  .gbmod { width: min(640px, 92vw); }
  .gbm-img { flex-shrink: 0; aspect-ratio: 16 / 9; max-height: 320px; margin: -6px -8px 12px; border-radius: 10px; overflow: hidden; background: var(--badge); }
  .gbm-img img { width: 100%; height: 100%; object-fit: cover; display: block; }
  .gbm-stats { display: flex; flex-wrap: wrap; gap: 4px 14px; margin: 2px 0 10px; color: var(--muted); font-size: 12.5px; font-variant-numeric: tabular-nums; }
  .gbm-text { flex: 1; min-height: 3em; max-height: 9.5em; overflow-y: auto; margin-bottom: 14px; }
  .gbm-text p { margin: 0; white-space: pre-line; overflow-wrap: anywhere; line-height: 1.45; }
  .tabs button { display: inline-flex; align-items: center; gap: 6px; padding: 5px 14px; font-size: 13px; }
  .tabs .count { min-width: 0; }
  .warn { display: flex; gap: 10px; align-items: flex-start; margin: 14px 14px 0; padding: 10px 12px; border-radius: 10px; background: rgb(245 166 35 / 0.12); border: 1px solid rgb(245 166 35 / 0.45); font-size: 13px; line-height: 1.45; }
  .warn p { margin: 0; }
  .warn :global(.icon), .gbm-warn :global(.icon) { flex-shrink: 0; width: 16px; height: 16px; margin-top: 1px; color: #e09a1a; }
  .cats-row { display: flex; overflow-x: auto; margin: 12px 14px 0; }
  .cats-row .seg { flex-wrap: wrap; gap: 2px; }
  .cats-row .count { margin-left: 4px; opacity: 0.7; font-size: 11px; font-variant-numeric: tabular-nums; }
  .dependency-notice { flex-shrink: 0; flex-wrap: wrap; align-items: center; margin: 10px 28px 0; }
  .dependency-notice p { flex: 1 1 240px; min-width: 0; }
  .dependency-notice button { white-space: normal; text-align: left; }
  .dependency-notice.ok { background: var(--ok-bg); border-color: var(--ok); }
  .dependency-notice.ok :global(.icon) { color: var(--ok); }
  .fw-status { color: var(--muted); }
  .fw-status.ok { color: var(--ok); }
  .gbm-warn { display: flex; gap: 8px; align-items: flex-start; margin: 0 0 12px; font-size: 12px; line-height: 1.4; color: var(--muted); }
  .gbm-warn.nsfw { color: #f59e0b; }
  .panel-head .seg button { padding: 2px 10px; }
  .info { flex: 1; min-width: 0; }
  .name, .sub { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .name { font-weight: 500; }
  .sub { font-size: 12px; color: var(--muted); }
  .sub :global(.icon) { width: 12px; height: 12px; vertical-align: -2px; margin-right: 4px; }
  .sub.run :global(.icon) { animation: rot 1s linear infinite; }
  .sub .src { border: 1px solid var(--border); border-radius: 4px; padding: 0 5px; margin-right: 4px; }
  .sub.bad { color: var(--danger); }
  .badge { background: var(--badge); color: var(--fg-soft); border-radius: 4px; padding: 1px 6px; font-size: 12px; margin-left: 6px; font-variant-numeric: tabular-nums; }
  .badge :global(.icon) { width: 11px; height: 11px; vertical-align: -1px; margin-right: 3px; }
  .badge.ok { background: var(--badge-ok); color: var(--fg); }
  .badge.tag { margin: 0 8px 0 0; font-size: 11px; }
  .badge.warn { background: rgb(245 166 35 / 0.18); color: #e09a1a; font-weight: 600; }
  .size { color: var(--muted); font-size: 12px; min-width: 60px; text-align: right; font-variant-numeric: tabular-nums; }
  .empty { margin: 12vh auto 0; max-width: 460px; display: flex; flex-direction: column; align-items: center; gap: 10px; padding: 28px; text-align: center; text-wrap: balance; }

  .nsz { display: flex; flex-direction: column; gap: 16px; padding: 20px 28px 24px; flex: 1; min-height: 0; }
  .toolbar { display: flex; align-items: center; gap: 10px; flex-shrink: 0; }
  .nsz-head { flex-shrink: 0; padding: 14px 16px; }
  .nsz-head > p { margin: 6px 0 0; }
  .what { font-size: 13px; color: var(--muted); }
  .what summary { cursor: pointer; display: inline-flex; align-items: center; gap: 4px; list-style: none; }
  .what summary::-webkit-details-marker { display: none; }
  .what[open] .chev { transform: rotate(90deg); }
  .what p { margin: 6px 0 0; }
  .del { padding: 8px 0 0; }
  .nsz-list { display: flex; flex-direction: column; gap: 12px; overflow-y: auto; flex: 1; min-height: 0; }
  .group > summary { list-style: none; cursor: pointer; color: var(--fg); }
  .group > summary::-webkit-details-marker { display: none; }
  .group:not([open]) > summary { border-bottom: 0; }
  .chev { display: inline-flex; color: var(--muted); transition: transform 0.15s; }
  .group[open] > summary .chev { transform: rotate(90deg); }
  .gname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .log { background: var(--input); padding: 10px; border-radius: 8px; white-space: pre-wrap; font-size: 12px; max-height: 120px; overflow: auto; margin: 8px 0 0; }
  .running { margin: 10px 0 0; padding: 10px 12px; border-radius: 8px; background: var(--input); border: 1px solid var(--border); }
  .run-top { display: flex; align-items: baseline; gap: 8px; margin-bottom: 6px; min-width: 0; }
  .run-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .track { height: 4px; border-radius: 2px; background: var(--border); overflow: hidden; }
  .fill { height: 100%; background: var(--fg); transition: width 0.3s; }
  .fill.indet { width: 35%; animation: slide 1.2s ease-in-out infinite alternate; }
  @keyframes slide { from { margin-left: 0; } to { margin-left: 65%; } }
  .nsz .quiet { background: transparent; color: var(--muted); opacity: 0; }
  .nsz .row:hover .quiet, .nsz .row:focus-within .quiet { opacity: 1; }
  .nsz .quiet:hover { color: var(--fg); background: var(--ghost); }
  .nsz .act { min-width: 108px; justify-content: center; background: transparent; color: var(--fg); border: 1px solid var(--border); }
  .nsz .act:hover:not(:disabled) { background: var(--ghost); }

  .toasts { position: fixed; left: 50%; bottom: 20px; transform: translateX(-50%); z-index: 10; display: flex; flex-direction: column; align-items: center; gap: 8px; width: min(440px, calc(100vw - 40px)); pointer-events: none; }
  .toasts > * { pointer-events: auto; width: 100%; }
  .card.failed { outline: 2px solid var(--danger); }
  .card-flag.bad { background: #b91c1c; color: #ffffff; border: 1px solid #ef4444; }
  .card-flag.nsfw { background: #b45309; color: #ffffff; border: 1px solid #f59e0b; }
  .toast { display: flex; align-items: center; gap: 10px; box-sizing: border-box; width: 100%; padding: 10px 12px 10px 16px; border-radius: 12px; background: var(--toast); color: var(--fg); border: 1px solid var(--border); box-shadow: var(--shadow); backdrop-filter: blur(16px); }
  .toast { transition: opacity 0.16s ease, transform 0.16s ease; }
  @starting-style { .toast { opacity: 0; transform: translateY(8px); } }
  .toast.err { border-color: var(--danger); box-shadow: 0 0 0 3px var(--danger-bg), var(--shadow); }
  .toast.err .ico { color: var(--danger); }
  .toast.ok { border-color: var(--ok); box-shadow: 0 0 0 3px var(--ok-bg), var(--shadow); }
  .toast.ok .ico { color: var(--ok); }
  .toast.dl { flex-direction: column; align-items: stretch; gap: 0; border-color: var(--accent); }
  .toast .x { margin-left: auto; background: transparent; color: inherit; padding: 4px; display: inline-flex; }
  .ico { display: inline-flex; }

  .modal { background: var(--toast); color: var(--fg); border: 1px solid var(--border); border-radius: 16px; padding: 20px 22px; width: min(520px, 90vw); max-height: 80vh; box-shadow: var(--shadow); }
  .modal[open] { display: flex; flex-direction: column; }
  .modal::backdrop { background: var(--backdrop); backdrop-filter: blur(4px); }
  .modal[open], .menu:popover-open { transition: opacity 0.16s ease, transform 0.16s ease; }
  @starting-style { .modal[open], .menu:popover-open { opacity: 0; transform: translateY(6px) scale(0.98); } }
  .modal h3 { margin: 0 0 4px; font-size: 16px; }
  .rm-modal { width: min(440px, 92vw); }
  .rm-header { display: flex; align-items: center; gap: 14px; margin-bottom: 12px; }
  .rm-badge { width: 42px; height: 42px; border-radius: 12px; background: rgba(239, 68, 68, 0.12); color: var(--danger); display: grid; place-items: center; flex-shrink: 0; }
  .rm-title-wrap { min-width: 0; }
  .rm-title-wrap h3 { margin: 0; font-size: 16px; font-weight: 600; }
  .rm-sub { margin: 2px 0 0; font-size: 13px; color: var(--fg-soft); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .rm-body { margin: 0 0 18px; font-size: 13.5px; line-height: 1.5; color: var(--fg-soft); }
  .rootlist { overflow-y: auto; display: flex; flex-direction: column; gap: 6px; margin: 8px 0; }
  .rootlist label { padding: 8px 10px; border-radius: 8px; background: var(--input); border: 1px solid var(--border-soft); }
  .rootlist label .muted { margin-left: auto; font-size: 12px; }
  .actions { display: flex; justify-content: flex-end; gap: 8px; }
  .guide ol { margin: 14px 0 18px; padding: 0; list-style: none; counter-reset: step; display: flex; flex-direction: column; gap: 14px; overflow-y: auto; }
  .guide li { counter-increment: step; display: grid; grid-template-columns: 24px 1fr; column-gap: 12px; }
  .guide li::before { content: counter(step); grid-row: span 2; width: 24px; height: 24px; border-radius: 50%; background: var(--badge); color: var(--fg-soft); font-size: 12px; font-weight: 600; display: grid; place-items: center; }
  .guide li > * { grid-column: 2; }
  .guide li b { font-weight: 600; }
  .upd-notes { max-height: 240px; overflow-y: auto; white-space: pre-wrap; font: inherit; font-size: 13px; margin: 8px 0 16px; padding: 10px 12px; background: var(--input); border: 1px solid var(--border-soft); border-radius: 8px; }
  .logo-btn { width: 38px; height: 38px; justify-content: center; padding: 0; margin-bottom: 10px; background: transparent; border-radius: 10px; }
  .logo-btn:hover:not(:disabled) { background: var(--ghost); }
  .menu { position: fixed; inset: auto; top: 12px; left: 62px; margin: 0; padding: 6px; min-width: 210px; background: var(--toast); color: var(--fg); border: 1px solid var(--border); border-radius: 12px; box-shadow: var(--shadow); }
  .menu:popover-open { display: flex; flex-direction: column; gap: 2px; }
  .menu button { background: transparent; justify-content: flex-start; width: 100%; padding: 8px 10px; font-size: 13px; color: var(--fg); }
  .menu button:hover { background: var(--ghost); }
  .settings { width: min(680px, 92vw); max-height: 86vh; }
  .modal:focus-visible { outline: none; }
  .sbody { overflow-y: auto; display: flex; flex-direction: column; margin: 8px -22px 16px; padding: 0 22px; }
  .dependencies { width: min(680px, calc(100vw - 64px)); }
  .dependencies .sbody { min-height: 0; line-height: 1.45; overflow-wrap: anywhere; }
  .dependencies .sbody > p { margin: 6px 0 12px; }
  .dependencies .warn { margin: 0 0 12px; }
  .dependencies ol { margin: 0 0 12px; padding-left: 24px; }
  .dependencies li { margin-bottom: 16px; }
  .dependencies li p { margin: 6px 0 10px; }
  .dependencies .btns button { white-space: normal; text-align: left; }
  .dependencies > h3, .dependencies > .actions { flex-shrink: 0; }
  .srow { display: grid; grid-template-columns: 150px 1fr; gap: 16px; padding: 14px 0; border-bottom: 1px solid var(--border-soft); }
  .srow:last-child { border-bottom: 0; }
  .srow h4 { margin: 0; padding-top: 4px; font-size: 13px; font-weight: 500; color: var(--fg-soft); }
  .sctl { display: flex; flex-direction: column; align-items: flex-start; gap: 8px; min-width: 0; }
  .sctl p { margin: 0; }
  .settings .actions { align-items: center; }
  .settings .actions p { margin: 0 auto 0 0; }
  .dirs { display: flex; flex-direction: column; gap: 8px; width: 100%; }
  .dirrow { display: flex; align-items: center; gap: 10px; }
  .dirrow b { width: 64px; flex-shrink: 0; font-weight: 600; }
  .dirrow .path { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .btns { display: flex; flex-wrap: wrap; gap: 8px; }
  .serr { color: var(--danger); font-size: 12px; }

  @media (forced-colors: active) {
    button, .badge, .row, .toast, .panel, .game, .search { border: 1px solid CanvasText; }
    .game.sel, .rail-btn[aria-current="page"], .seg button[aria-pressed="true"] { outline: 2px solid Highlight; }
    .fill { background: Highlight; }
  }
  @media (prefers-reduced-motion: reduce) { .fill.indet, .card-fill.indet, .card.skel, .spinner { animation: none; } .fill.indet { width: 100%; opacity: 0.5; } .rail-btn.spin :global(.icon), .sub.run :global(.icon) { animation: none; } .chev, button, .row, .modal, .menu, .toast, .card, .card-img img, .card-fill { transition: none; } .card:hover { transform: none; } .card:hover .card-img img { transform: none; } button:active { transform: none; } }
</style>
