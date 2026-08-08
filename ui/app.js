// SmartClip Studio — logique de la vue.
//
// Aucune règle métier ici : les indices de pistes, le filtrage des emplacements
// vacants et le mixage viennent tous du moteur. Cette couche ne fait que
// présenter et transmettre.

import { Preview } from "./preview.js";

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const el = (id) => document.getElementById(id);
const state = { recording: false, clip: null, gains: new Map() };
let preview;

// ─────────────────────────────── utilitaires ───────────────────────────────

let toastTimer;
function toast(message, isError = false) {
  const node = el("toast");
  node.textContent = message;
  node.classList.toggle("error", isError);
  node.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => node.classList.remove("show"), 4000);
}

async function call(command, args) {
  try {
    return await invoke(command, args);
  } catch (error) {
    toast(String(error), true);
    throw error;
  }
}

const formatDuration = (seconds) => {
  const total = Math.round(seconds);
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
};

const formatDate = (epoch) =>
  new Date(epoch * 1000).toLocaleString("fr-FR", {
    day: "numeric", month: "short", hour: "2-digit", minute: "2-digit",
  });

// ────────────────────────────── état du buffer ─────────────────────────────

function renderStatus(tracks) {
  el("status").classList.toggle("live", state.recording);
  el("statusText").textContent = state.recording
    ? `Enregistre — ${tracks.filter((t) => !t.startsWith("(libre")).join(", ")}`
    : "Buffer arrêté";
  el("toggle").textContent = state.recording ? "Arrêter" : "Démarrer le buffer";
  el("save").disabled = !state.recording;
}

async function toggleRecording() {
  const button = el("toggle");
  button.disabled = true;
  try {
    if (state.recording) {
      await call("stop_recording");
      state.recording = false;
      renderStatus([]);
    } else {
      // Le démarrage ouvre la capture et découvre les sources : compter
      // quelques centaines de millisecondes, d'où le bouton désactivé.
      const tracks = await call("start_recording");
      state.recording = true;
      renderStatus(tracks);
      toast("Buffer démarré — les dernières secondes sont désormais récupérables");
    }
  } finally {
    button.disabled = false;
  }
}

async function saveClip() {
  const button = el("save");
  button.disabled = true;
  // Une sauvegarde n'est pas instantanée : ~1,3 s pour une minute de buffer,
  // ~4 s pour cinq. Sans retour visuel, l'utilisateur croirait à un blocage et
  // insisterait sur le bouton.
  const previous = button.textContent;
  button.textContent = "Sauvegarde…";
  el("status").classList.add("saving");
  try {
    const outcome = await call("save_clip");
    toast(`Clip de ${formatDuration(outcome.seconds)} sauvegardé en ${Math.round(outcome.total_ms)} ms`);
    await loadLibrary();
  } finally {
    button.textContent = previous;
    el("status").classList.remove("saving");
    button.disabled = !state.recording;
  }
}

// ─────────────────────────────── bibliothèque ──────────────────────────────

/// Clips tels que le moteur les a rendus, avant recherche et tri.
let library = [];
let query = "";
let sortMode = "recent";

const SORTS = {
  recent: (a, b) => b.created - a.created,
  long: (a, b) => b.seconds - a.seconds,
  heavy: (a, b) => b.megabytes - a.megabytes,
};

async function loadLibrary() {
  library = await call("list_clips");
  renderLibrary();
}

/// Applique recherche et tri, puis reconstruit la grille.
///
/// La recherche porte aussi sur les libellés de pistes : « discord » retrouve
/// les clips où l'on parlait, ce qu'un nom de fichier horodaté ne dira jamais.
function renderLibrary() {
  const needle = query.trim().toLowerCase();
  const clips = library
    .filter(
      (clip) =>
        needle === "" ||
        clip.name.toLowerCase().includes(needle) ||
        clip.tracks.some((track) => track.label.toLowerCase().includes(needle)),
    )
    .sort(SORTS[sortMode] ?? SORTS.recent);

  const total = library.reduce((sum, clip) => sum + clip.megabytes, 0);
  el("clipCount").textContent =
    `${library.length} clip${library.length > 1 ? "s" : ""}`;
  el("clipSize").textContent = library.length === 0 ? "" : space(total / 1024);

  el("libraryBar").classList.toggle("hidden", library.length === 0);
  el("empty").classList.toggle("hidden", library.length > 0);
  el("noMatch").classList.toggle("hidden", library.length === 0 || clips.length > 0);

  const grid = el("grid");
  grid.innerHTML = "";

  clips.forEach((clip, position) => {
    const card = document.createElement("div");
    card.className = "card";
    // Cascade plafonnée : au-delà d'une douzaine de cartes, l'attente cesse
    // d'être élégante pour devenir une lenteur.
    card.style.animationDelay = `${Math.min(position, 12) * 35}ms`;
    card.onclick = () => {
      // La carte se soulève avant que l'éditeur prenne la place : sans ce
      // court délai, le mouvement n'a pas le temps d'être perçu.
      card.classList.add("opening");
      setTimeout(() => openEditor(clip), 130);
    };

    // Vignette extraite cote Rust a la sauvegarde, puis conservee a cote du
    // clip. La faire ici imposait un decodeur video par carte, ce qui
    // degradait la lecture, et le canvas etait de toute facon teinte par le
    // protocole asset.
    const cover = document.createElement("div");
    cover.className = "cover";
    if (clip.thumbnail) {
      cover.style.backgroundImage = `url(${convertFileSrc(clip.thumbnail)})`;
      cover.classList.add("has-image");
    }

    const duration = document.createElement("span");
    duration.className = "cover-duration";
    duration.textContent = formatDuration(clip.seconds);

    const count = document.createElement("span");
    count.className = "cover-tracks";
    count.textContent = `${clip.tracks.length} piste${clip.tracks.length > 1 ? "s" : ""}`;
    cover.append(count, duration);

    const title = document.createElement("h3");
    title.textContent = clip.name;

    const meta = document.createElement("div");
    meta.className = "meta";
    meta.textContent =
      `${formatDuration(clip.seconds)} · ${clip.megabytes.toFixed(0)} Mo · ${formatDate(clip.created)}`;

    const chips = document.createElement("div");
    chips.className = "chips";
    for (const track of clip.tracks) {
      const chip = document.createElement("span");
      chip.className = "chip";
      chip.textContent = track.label;
      chips.appendChild(chip);
    }

    card.append(cover, title, meta, chips);
    grid.appendChild(card);
  });
}

// ──────────────────────────────── éditeur ──────────────────────────────────

async function openEditor(clip) {
  state.clip = clip;
  state.gains = new Map(clip.tracks.map((t) => [t.index, 1]));

  el("libraryView").classList.add("hidden");
  el("editorView").classList.remove("hidden");
  el("back").classList.remove("hidden");
  el("clipName").textContent = clip.name;
  // La vidéo est coupée AVANT d'être chargée.
  //
  // Le lecteur du webview joue d'office une des pistes audio du MP4 — la
  // première, choisie sans rapport avec les réglages. La couper seulement après
  // le chargement des pistes laissait passer ce son, y compris tous les faders
  // à zéro : on entendait alors une piste qu'on croyait muette.
  el("player").muted = true;
  el("player").src = convertFileSrc(clip.path);

  const container = el("tracks");
  container.innerHTML = "";
  // Réactivé seulement après analyse des pistes du nouveau clip.
  el("autoMix").disabled = true;

  if (clip.tracks.length === 0) {
    const note = document.createElement("div");
    note.className = "hint";
    note.textContent = "Ce clip n'a pas de métadonnées : ses pistes ne peuvent pas être nommées.";
    container.appendChild(note);
    return;
  }

  trackRows.clear();
  for (const track of clip.tracks) {
    const row = buildTrack(track);
    trackRows.set(track.index, row);
    container.appendChild(row);
  }

  // L'extraction des pistes prend le temps de décoder l'audio : l'éditeur
  // s'affiche tout de suite et l'écoute s'active quand elle est prête.
  el("previewState").textContent = "Préparation de l'écoute…";
  try {
    const tracks = await invoke("prepare_preview", { source: clip.path });
    if (tracks.length === 0) throw new Error("aucune piste exploitable dans ce clip");
    await preview.load(tracks, convertFileSrc);
    for (const [index, gain] of state.gains) preview.setGain(index, gain);
    // Une piste vide n'est pas une panne : l'application était ouverte mais ne
    // jouait rien. Le dire évite de chercher un défaut là où il n'y en a pas.
    const silent = new Set(preview.silentTracks());
    for (const [index, row] of trackRows) {
      if (silent.has(index)) markSilent(row);
    }
    const audible = tracks.length - silent.size;
    el("previewState").textContent =
      silent.size > 0
        ? `Écoute en direct — ${audible} piste(s) avec du son, ${silent.size} silencieuse(s)`
        : `Écoute en direct sur ${tracks.length} piste(s) — bouge un fader pendant la lecture`;
    el("previewState").classList.remove("warn");

    // Doublons de signal : Discord réémet souvent le micro. Sans avertissement,
    // l'utilisateur s'entend en double sans comprendre pourquoi.
    for (const pair of preview.duplicatePairs()) {
      const nameOf = (i) => clip.tracks.find((t) => t.index === i)?.label ?? `piste ${i}`;
      for (const index of [pair.a, pair.b]) {
        const row = trackRows.get(index);
        if (row) row.classList.add("duplicate");
      }
      toast(
        `« ${nameOf(pair.a)} » et « ${nameOf(pair.b)} » contiennent le même son — ` +
          `cumuler les deux le fera entendre en double. Coupe l'une des deux.`,
        true,
      );
    }
    // L'équilibrage et la forme d'onde n'ont de sens qu'une fois les pistes
    // analysées.
    el("autoMix").disabled = false;
    envelope = preview.waveform();
    drawWaveform();
    if (!el("player").paused) preview.start();
  } catch (error) {
    // Sans écoute en direct, l'éditeur reste utilisable : les faders agissent
    // toujours à l'export. Mais il faut le dire, et fort.
    //
    // Cet échec était auparavant journalisé dans une console invisible : la
    // vidéo restait non-muette et jouait sa piste par défaut, les faders
    // pilotaient un mixeur vide, et rien n'expliquait pourquoi.
    // Sans écoute en direct, on rétablit le son du lecteur : mieux vaut
    // entendre une seule piste que rien du tout.
    el("player").muted = false;
    el("previewState").textContent =
      "⚠ Écoute en direct indisponible — les réglages s'appliqueront quand même à l'export";
    el("previewState").classList.add("warn");
    toast(`Écoute en direct impossible : ${error}`, true);
  }
}

/// Lignes de piste affichées, indexées par numéro de piste dans le fichier.
const trackRows = new Map();

/// Signale qu'une piste ne contient aucun son.
function markSilent(row) {
  row.classList.add("silent");
  const value = row.querySelector(".value");
  if (value) value.textContent = "aucun son";
}

function buildTrack(track) {
  const row = document.createElement("div");
  row.className = "track";

  // Vumètre : une barre fine sous le libellé, qui vit pendant la lecture.
  const meter = document.createElement("div");
  meter.className = "meter";
  const level = document.createElement("span");
  meter.appendChild(level);

  const head = document.createElement("div");
  head.className = "track-head";

  const mute = document.createElement("button");
  mute.className = "mute";
  mute.textContent = "♪";
  mute.title = "Couper cette piste";

  const label = document.createElement("span");
  label.textContent = track.label;

  const value = document.createElement("span");
  value.className = "value";
  value.textContent = "100 %";

  head.append(mute, label, value);

  const slider = document.createElement("input");
  slider.type = "range";
  slider.min = "0";
  // Jusqu'à 200 % : une piste enregistrée trop bas doit pouvoir être remontée,
  // c'est précisément le cas d'usage du micro trop faible.
  slider.max = "200";
  slider.value = "100";

  let muted = false;
  const apply = () => {
    const gain = muted ? 0 : Number(slider.value) / 100;
    state.gains.set(track.index, gain);
    // Le gain part immédiatement vers l'écoute : c'est ce qui rend le réglage
    // audible pendant qu'on le fait, au lieu d'exiger un export pour juger.
    preview.setGain(track.index, gain);
    value.textContent = muted ? "coupée" : `${slider.value} %`;
    row.classList.toggle("muted", muted);
  };

  slider.oninput = () => { muted = false; mute.classList.remove("on"); apply(); };
  mute.onclick = () => { muted = !muted; mute.classList.toggle("on", muted); apply(); };

  row.append(head, meter, slider);
  return row;
}

// ─────────────────────────── forme d'onde ──────────────────────────────────

/// Enveloppe du clip courant, calculée une fois au chargement.
let envelope = null;

/// Dessine la forme d'onde et la position de lecture.
///
/// Deux couches : l'enveloppe complète en sourdine, et la portion déjà lue en
/// couleur. On voit d'un coup d'œil où sont les moments forts et où l'on en
/// est — ce qu'une barre de progression seule ne dit pas.
function drawWaveform() {
  const canvas = el("waveform");
  const context = canvas.getContext("2d");
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  if (width === 0 || height === 0) return;

  if (canvas.width !== width * ratio) {
    canvas.width = width * ratio;
    canvas.height = height * ratio;
  }
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  if (!envelope) return;

  const player = el("player");
  const progress = player.duration > 0 ? player.currentTime / player.duration : 0;
  const middle = height / 2;
  const step = width / envelope.length;

  for (let i = 0; i < envelope.length; i++) {
    const x = i * step;
    // Une hauteur minimale garde une ligne continue dans les silences : un
    // trou complet se lirait comme une coupure du clip.
    const amplitude = Math.max(1.5, envelope[i] * (height / 2 - 2));
    context.fillStyle = x / width <= progress ? "#5b7cfa" : "#2b2e33";
    context.fillRect(x, middle - amplitude, Math.max(1, step - 0.5), amplitude * 2);
  }

  // Tête de lecture.
  const head = progress * width;
  context.fillStyle = "#f2f3f5";
  context.fillRect(head - 1, 0, 2, height);
}

/// Applique l'équilibre proposé aux faders.
///
/// Les curseurs bougent réellement : l'utilisateur voit ce qui a été décidé
/// pour lui et peut le corriger. Un réglage invisible serait à la fois moins
/// utile et moins fiable.
function applyAutoMix() {
  const labels = new Map(state.clip.tracks.map((t) => [t.index, t.label]));
  const suggested = preview.suggestGains(labels);
  if (suggested.size === 0) {
    toast("Aucune piste audible à équilibrer", true);
    return;
  }

  for (const [index, gain] of suggested) {
    const row = trackRows.get(index);
    const slider = row?.querySelector('input[type="range"]');
    if (!slider) continue;
    slider.value = String(Math.round(gain * 100));
    // `input` déclenche la logique existante : mémorisation, écoute, libellé.
    slider.dispatchEvent(new Event("input"));
  }
  toast(`${suggested.size} piste(s) équilibrée(s) — ajuste si besoin`);
}

async function exportMix() {
  const button = el("export");
  button.disabled = true;
  button.textContent = "Export en cours…";
  try {
    const outcome = await call("export_mix", {
      source: state.clip.path,
      gains: [...state.gains.entries()],
    });
    toast(
      outcome.clipped
        ? `Exporté, mais le mixage saturait (crête ${outcome.peak.toFixed(2)}) — baisse les pistes les plus fortes`
        : `Exporté : ${outcome.path.split("\\").pop()} (${outcome.megabytes.toFixed(0)} Mo)`,
      outcome.clipped,
    );
    await loadLibrary();
  } finally {
    button.disabled = false;
    button.textContent = "Exporter";
  }
}

async function deleteClip() {
  await call("delete_clip", { path: state.clip.path });
  toast("Clip supprimé");
  // `closeEditor` reconstruit déjà la bibliothèque.
  await closeEditor();
}

async function closeEditor() {
  envelope = null;
  preview.unload();
  if (state.clip) invoke("clear_preview", { source: state.clip.path }).catch(() => {});
  // Couper la source libère le fichier : sans cela Windows le garde verrouillé
  // et la suppression échoue.
  el("player").pause();
  el("player").removeAttribute("src");
  el("player").load();
  el("editorView").classList.add("hidden");
  el("libraryView").classList.remove("hidden");
  el("back").classList.add("hidden");
  state.clip = null;
  // Les aperçus ont été libérés à l'ouverture de l'éditeur : la bibliothèque
  // est reconstruite pour les rétablir.
  await loadLibrary();
}

// ─────────────────────────────── réglages ──────────────────────────────────

let draftBuffer = 60;
let draftQuality = "haute";
/// Élément qui avait le focus avant l'ouverture, pour le lui rendre en sortant.
let focusBeforeSettings = null;

async function openSettings() {
  const settings = await call("get_settings");
  draftBuffer = settings.buffer_seconds;
  draftQuality = settings.quality ?? "haute";
  el("outputDir").value = settings.output_dir;
  el("hotkey").value = settings.hotkey;
  el("voicePhrase").value = settings.voice_phrase ?? "";
  el("autoStart").checked = settings.auto_start ?? true;
  el("launchAtLogin").checked = settings.launch_at_login ?? false;
  el("captureMic").checked = settings.capture_microphone ?? true;
  el("overlayBadge").checked = settings.overlay ?? true;
  el("diskCap").value = String(settings.max_gigabytes ?? 3);
  // Rend aussi les choix : l'estimation de place dépend du plafond.
  renderDiskCap();
  await renderMicrophones(settings.microphone ?? "");
  await renderHotkeyState();
  await renderVoiceState();

  focusBeforeSettings = document.activeElement;
  el("settingsPanel").classList.remove("hidden");
  selectTab("capture");
}

function closeSettings() {
  el("settingsPanel").classList.add("hidden");
  // Le focus revient d'où il venait : sans cela il retombe sur le corps du
  // document et la navigation au clavier repart du début de la page.
  focusBeforeSettings?.focus?.();
}

/// Affiche un onglet et lui donne le focus clavier.
function selectTab(name) {
  for (const tab of el("settingsPanel").querySelectorAll('[role="tab"]')) {
    const active = tab.dataset.tab === name;
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
  }
  for (const panel of el("settingsPanel").querySelectorAll('[role="tabpanel"]')) {
    panel.hidden = panel.dataset.panel !== name;
  }
}

/// Remplit la liste des micros, et signale un périphérique disparu.
///
/// Le micro choisi peut avoir été débranché depuis le réglage. Le moteur se
/// replie alors sur celui de Windows ; le taire donnerait une piste voix
/// enregistrée sur un périphérique qu'on n'a pas choisi, sans explication.
async function renderMicrophones(selected) {
  const select = el("microphone");
  const note = el("micState");
  select.innerHTML = "";

  const devices = await invoke("list_microphones").catch(() => null);
  if (!devices) {
    select.innerHTML = '<option value="">Micro par défaut de Windows</option>';
    note.textContent = "⚠ Impossible de lire la liste des micros.";
    note.classList.add("warn");
    return;
  }

  const auto = document.createElement("option");
  auto.value = "";
  auto.textContent = devices.find((d) => d.default)
    ? `Micro par défaut — ${devices.find((d) => d.default).name}`
    : "Micro par défaut de Windows";
  select.appendChild(auto);

  for (const device of devices) {
    const option = document.createElement("option");
    option.value = device.id;
    option.textContent = device.name;
    select.appendChild(option);
  }

  const known = devices.some((device) => device.id === selected);
  select.value = known ? selected : "";
  if (selected && !known) {
    note.textContent =
      "⚠ Le micro choisi n'est plus connecté — celui de Windows est utilisé à la place.";
    note.classList.add("warn");
  } else if (devices.length === 0) {
    note.textContent = "⚠ Aucun micro actif détecté sur cette machine.";
    note.classList.add("warn");
  } else {
    note.textContent = `${devices.length} micro(s) détecté(s).`;
    note.classList.remove("warn");
  }
}

/// Affiche si le raccourci est réellement pris en compte par Windows.
///
/// Un raccourci refusé — combinaison déjà utilisée par un autre logiciel —
/// rendait la fonction principale du produit inutilisable sans le moindre
/// signe. C'est la première chose que l'utilisateur doit pouvoir vérifier.
async function renderHotkeyState() {
  const status = await invoke("hotkey_status").catch(() => null);
  const node = el("hotkeyState");
  if (!status) {
    node.textContent = "";
    return;
  }
  node.textContent = status.registered
    ? `✓ ${status.combination} est actif`
    : `⚠ ${status.error ?? "raccourci inactif"} — essaie une autre combinaison`;
  node.classList.toggle("warn", !status.registered);
}

/// Débit disque réellement mesuré, en Mo/s, par niveau de qualité.
///
/// Ce ne sont pas les débits demandés : l'encodeur matériel AMD produit
/// couramment le double de sa consigne. Annoncer la théorie tromperait
/// l'utilisateur sur la place occupée, qui est la vraie contrainte du produit.
const DISK_RATE = { haute: 5.3, equilibree: 3.2, legere: 2.1 };

function renderChoices() {
  for (const button of el("bufferChoices").children) {
    button.classList.toggle("active", Number(button.dataset.seconds) === draftBuffer);
  }
  for (const button of el("qualityChoices").children) {
    button.classList.toggle("active", button.dataset.quality === draftQuality);
  }

  // Trois coûts croissent avec la durée et la qualité, et l'utilisateur doit
  // les connaître avant de choisir : la place occupée, le temps de sauvegarde
  // (proportionnel au nombre de segments à recoller) et le plafond disque, qui
  // tronque le buffer s'il est trop bas.
  const rate = DISK_RATE[draftQuality] ?? DISK_RATE.haute;
  const gigabytes = (draftBuffer * rate) / 1024;
  const saveSeconds = decimal(Math.max(0.5, (draftBuffer / 60) * 1.3));
  const cap = Number(el("diskCap").value);

  let text =
    `Environ <b>${space(gigabytes)}</b> occupés en continu, ` +
    `et ~${saveSeconds} s par sauvegarde.`;
  if (gigabytes > cap) {
    // Le plafond l'emporte sur la durée : le dire évite de croire qu'on couvre
    // cinq minutes alors que le buffer en garde deux.
    const kept = Math.round((cap * 1024) / rate);
    text +=
      ` <b>Le plafond de ${decimal(cap)} Go tronquera le buffer à ~${kept} s</b> —` +
      ` monte-le, ou baisse la qualité.`;
  }
  el("bufferHint").innerHTML = text;
}

/// Un décimal à la française : la virgule, et pas de zéro inutile.
const decimal = (value) =>
  value.toLocaleString("fr-FR", { maximumFractionDigits: 1 });

/// Une taille dans l'unité où le chiffre reste parlant. « 0,3 Go » se lit moins
/// bien que « 311 Mo ».
const space = (gigabytes) =>
  gigabytes < 1
    ? `${Math.round(gigabytes * 1024)} Mo`
    : `${decimal(gigabytes)} Go`;

function renderDiskCap() {
  el("diskCapValue").textContent = `${decimal(Number(el("diskCap").value))} Go`;
  renderChoices();
}

/// Affiche l'état de l'écoute vocale.
///
/// Elle dépend de la reconnaissance vocale de Windows, qui peut manquer — pack
/// de langue absent, fonctionnalité désactivée. Le silence serait pire que
/// l'absence de fonction : l'utilisateur répéterait sa phrase sans savoir
/// pourquoi rien ne se passe.
async function renderVoiceState() {
  const status = await invoke("voice_status").catch(() => null);
  const node = el("voiceState");
  if (!status || !status.combination) {
    node.textContent = "Laisse vide pour ne pas écouter le micro.";
    node.classList.remove("warn");
    return;
  }
  node.textContent = status.registered
    ? `✓ Dis « ${status.combination} » pour sauvegarder`
    : `⚠ ${status.error ?? "écoute vocale indisponible"}`;
  node.classList.toggle("warn", !status.registered);
}

async function saveSettings() {
  const running = state.recording;
  await call("set_settings", {
    settings: {
      buffer_seconds: draftBuffer,
      output_dir: el("outputDir").value.trim(),
      hotkey: el("hotkey").value.trim() || "Ctrl+Shift+X",
      voice_phrase: el("voicePhrase").value.trim(),
      auto_start: el("autoStart").checked,
      launch_at_login: el("launchAtLogin").checked,
      capture_microphone: el("captureMic").checked,
      microphone: el("microphone").value,
      quality: draftQuality,
      max_gigabytes: Number(el("diskCap").value),
      overlay: el("overlayBadge").checked,
    },
  });
  await renderVoiceState();

  // Le raccourci est réappliqué immédiatement : si Windows le refuse, autant
  // le savoir avant de retourner jouer.
  const status = await invoke("hotkey_status").catch(() => null);
  if (status && !status.registered) {
    toast(`Raccourci non pris en compte : ${status.error}`, true);
    selectTab("declencheurs");
    await renderHotkeyState();
    return;
  }
  closeSettings();

  // Durée, qualité, plafond et micro sont tous fixés à l'ouverture de la
  // capture : sans redémarrage ils ne prendraient effet qu'à la prochaine
  // session, ce qui serait incompréhensible.
  if (running) {
    await call("stop_recording");
    const tracks = await call("start_recording");
    state.recording = true;
    renderStatus(tracks);
    toast("Réglages appliqués — buffer redémarré");
  } else {
    toast("Réglages enregistrés");
  }
  await loadLibrary();
}

// ─────────────────────────────── démarrage ─────────────────────────────────

el("toggle").onclick = toggleRecording;
el("save").onclick = saveClip;
el("back").onclick = closeEditor;
el("autoMix").onclick = applyAutoMix;
el("export").onclick = exportMix;
el("delete").onclick = deleteClip;

// ── écoute en direct, calée sur le lecteur ──
preview = new Preview(el("player"));
const player = el("player");
player.addEventListener("play", () => preview.start());
player.addEventListener("pause", () => preview.stop());
player.addEventListener("seeked", () => { if (!player.paused) preview.start(); });
player.addEventListener("ended", () => preview.stop());
// Surveillance de la dérive : les horloges du webview et de la carte son sont
// indépendantes, un écart s'installe lentement et doit être rattrapé.
setInterval(() => preview.tick(), 500);

// Animation des vumètres, calée sur le rafraîchissement de l'écran.
//
// Une boucle d'animation ne coûte rien quand rien ne joue : `levels()` rend
// une table vide et l'on sort aussitôt. Le navigateur la suspend d'ailleurs
// dès que la fenêtre passe en arrière-plan.
function animateMeters() {
  const levels = preview?.levels?.() ?? new Map();
  for (const [index, level] of levels) {
    const bar = trackRows.get(index)?.querySelector(".meter span");
    if (bar) bar.style.transform = `scaleX(${level.toFixed(3)})`;
  }
  if (envelope) drawWaveform();
  requestAnimationFrame(animateMeters);
}
requestAnimationFrame(animateMeters);

// Navigation par clic sur la forme d'onde : on vise un pic, on y va.
el("waveform").onclick = (event) => {
  const player = el("player");
  if (!player.duration) return;
  const bounds = event.currentTarget.getBoundingClientRect();
  player.currentTime = ((event.clientX - bounds.left) / bounds.width) * player.duration;
};

// ── bibliothèque : recherche et tri ──
el("search").oninput = (event) => { query = event.target.value; renderLibrary(); };
for (const button of el("sort").children) {
  button.onclick = () => {
    sortMode = button.dataset.sort;
    for (const other of el("sort").children) {
      other.classList.toggle("active", other === button);
    }
    renderLibrary();
  };
}

// ── réglages ──
el("settings").onclick = openSettings;
el("settingsCancel").onclick = closeSettings;
el("settingsSave").onclick = saveSettings;
el("settingsPanel").onclick = (event) => {
  if (event.target === el("settingsPanel")) closeSettings();
};
for (const button of el("bufferChoices").children) {
  button.onclick = () => { draftBuffer = Number(button.dataset.seconds); renderChoices(); };
}
for (const button of el("qualityChoices").children) {
  button.onclick = () => { draftQuality = button.dataset.quality; renderChoices(); };
}
el("diskCap").oninput = renderDiskCap;

// Onglets : clic, et flèches gauche/droite comme l'attend un lecteur d'écran.
const tabButtons = [...el("settingsPanel").querySelectorAll('[role="tab"]')];
tabButtons.forEach((tab, position) => {
  tab.onclick = () => selectTab(tab.dataset.tab);
  tab.onkeydown = (event) => {
    const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (step === 0) return;
    event.preventDefault();
    const next = tabButtons[(position + step + tabButtons.length) % tabButtons.length];
    selectTab(next.dataset.tab);
    next.focus();
  };
});

// Échap ferme ce qui est ouvert : d'abord la boîte de réglages, sinon
// l'éditeur. Une fenêtre dont on ne sort qu'à la souris paraît toujours lourde.
document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  if (!el("settingsPanel").classList.contains("hidden")) {
    closeSettings();
  } else if (!el("editorView").classList.contains("hidden")) {
    closeEditor();
  }
});

// ── sauvegardes déclenchées hors de la vue (raccourci, barre système) ──
listen("clip-saved", async (event) => {
  toast(
    `Clip de ${formatDuration(event.payload.seconds)} sauvegardé en ${Math.round(event.payload.total_ms)} ms`,
  );
  await loadLibrary();
});
listen("save-failed", (event) => toast(String(event.payload), true));

// Les vignettes rattrapées apparaissent sans avoir à rouvrir l'application.
listen("thumbnails-ready", async (event) => {
  await loadLibrary();
  toast(`${event.payload} aperçu(s) de clip ajouté(s)`);
});

// ── surveillance du buffer ──
//
// Un buffer arrêté sans que l'utilisateur le sache est le pire défaut possible :
// il jouerait des heures en se croyant couvert. On vérifie donc l'état à
// intervalle régulier plutôt que d'attendre l'appui sur le raccourci.
let lastRestarts = 0;
setInterval(async () => {
  if (!state.recording) return;
  const health = await invoke("recorder_health").catch(() => null);
  if (!health) return;
  if (!health.running) {
    state.recording = false;
    renderStatus([]);
    toast(health.failure ?? "Le buffer s'est arrêté", true);
  } else if (health.stalled) {
    // Le buffer se déclare actif mais n'a plus traité d'image : sans cette
    // alerte, l'utilisateur jouerait des heures en se croyant couvert.
    el("status").classList.remove("live");
    el("statusText").textContent =
      `⚠ Buffer figé depuis ${Math.round(health.stalled_ms / 1000)} s`;
    toast("Le buffer ne progresse plus — arrête puis redémarre l'enregistrement", true);
  } else if (health.restarts > lastRestarts) {
    // Un changement de définition vide le buffer : le dire évite de croire
    // qu'on couvre encore les minutes précédentes.
    lastRestarts = health.restarts;
    toast("La définition de l'écran a changé — le buffer est reparti de zéro", true);
    el("statusText").textContent = "Enregistre — reparti après changement d'écran";
  } else if (health.write_errors > 0) {
    el("statusText").textContent = `Enregistre — ${health.write_errors} image(s) perdue(s)`;
  }
}, 3000);

(async () => {
  state.recording = await call("is_recording");
  renderStatus(state.recording ? await call("current_tracks") : []);
  await loadLibrary();

  // Vignettes manquantes des clips antérieurs, rattrapées en fond. La grille
  // s'affiche d'abord : attendre les extractions retarderait tout l'écran pour
  // une amélioration cosmétique.
  invoke("backfill_thumbnails").catch(() => {});

  // Démarrage automatique du buffer.
  //
  // Un clip manqué parce qu'on avait oublié de lancer l'enregistrement est le
  // pire défaut possible pour ce produit : l'occasion ne repasse pas. Le coût
  // inverse — un buffer qui tourne pour rien — se compte en quelques pour cent
  // de processeur.
  const settings = await invoke("get_settings").catch(() => null);
  if (settings?.auto_start && !state.recording) {
    try {
      const tracks = await invoke("start_recording");
      state.recording = true;
      renderStatus(tracks);
    } catch (error) {
      toast(`Démarrage automatique impossible : ${error}`, true);
    }
  }
})();
