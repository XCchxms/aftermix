//! SmartClip Studio — interface.
//!
//! Cette couche ne contient aucune logique multimédia : elle expose le moteur à
//! la vue et rien de plus. Tout ce qui touche à la capture, au mixage ou aux
//! métadonnées vit dans `smartclip-engine`, où il reste testable sans fenêtre.

// Empêche la console d'apparaître derrière la fenêtre en release. En debug on
// la garde : c'est là que sortent les traces du moteur.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use smartclip_engine::{Config, Recorder, library};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// Un clip tel que la vue l'affiche.
#[derive(Serialize)]
struct ClipView {
    path: String,
    name: String,
    seconds: f64,
    megabytes: f64,
    created: u64,
    /// Pistes réellement occupées : les emplacements vacants sont retirés ici
    /// plutôt que dans la vue, pour qu'aucune interface future n'ait à
    /// reproduire la règle.
    tracks: Vec<TrackView>,
}

#[derive(Serialize)]
struct TrackView {
    /// Indice réel dans le fichier — c'est lui qui sert à composer les gains,
    /// et il ne coïncide pas avec la position affichée dès qu'un emplacement
    /// vacant a été retiré.
    index: usize,
    label: String,
}

/// Réglages modifiables depuis l'interface.
#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    buffer_seconds: f64,
    output_dir: String,
    /// Raccourci de sauvegarde, au format `Ctrl+Shift+X`.
    ///
    /// Personnalisable : la combinaison par défaut entre en conflit avec
    /// d'autres logiciels — overlays de constructeurs, suites de streaming — et
    /// l'utilisateur doit pouvoir en choisir une autre sans quoi la fonction
    /// principale du produit lui reste inaccessible.
    #[serde(default = "default_hotkey")]
    hotkey: String,
    /// Phrase d'activation vocale. Vide = écoute désactivée.
    #[serde(default)]
    voice_phrase: String,
}

fn default_hotkey() -> String {
    "Ctrl+Shift+X".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        let videos = std::env::var("USERPROFILE")
            .map(|home| PathBuf::from(home).join("Videos").join("SmartClip"))
            .unwrap_or_else(|_| std::env::temp_dir().join("SmartClip"));
        Self {
            buffer_seconds: 60.0,
            output_dir: videos.to_string_lossy().into_owned(),
            hotkey: default_hotkey(),
            // Désactivée par défaut : écouter le micro en permanence doit
            // rester un choix explicite de l'utilisateur.
            voice_phrase: String::new(),
        }
    }
}

impl Settings {
    fn file() -> Option<PathBuf> {
        let base = std::env::var("APPDATA").ok()?;
        Some(PathBuf::from(base).join("SmartClip").join("settings.json"))
    }

    /// Relit les réglages, ou rend ceux par défaut.
    ///
    /// Un fichier illisible ou corrompu ne bloque pas le démarrage : mieux vaut
    /// repartir des valeurs par défaut que refuser d'ouvrir l'application.
    fn load() -> Self {
        Self::file()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn persist(&self) -> anyhow::Result<()> {
        let path = Self::file().ok_or_else(|| anyhow::anyhow!("APPDATA introuvable"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// État d'enregistrement du raccourci, tel que l'interface doit l'afficher.
#[derive(Serialize, Clone, Default)]
struct HotkeyStatus {
    combination: String,
    registered: bool,
    /// Renseigné quand Windows a refusé la combinaison — presque toujours parce
    /// qu'une autre application l'a déjà prise.
    error: Option<String>,
}

struct AppState {
    recorder: Mutex<Option<Recorder>>,
    settings: Mutex<Settings>,
    hotkey: Mutex<HotkeyStatus>,
    voice: Mutex<Option<smartclip_engine::voice::VoiceTrigger>>,
    voice_status: Mutex<HotkeyStatus>,
}

/// (Ré)active l'écoute vocale, et mémorise le résultat.
///
/// Une phrase vide coupe l'écoute : c'est le réglage par défaut, car écouter le
/// micro en permanence doit rester un choix explicite.
fn apply_voice(app: &AppHandle, phrase: &str) -> HotkeyStatus {
    let state = app.state::<AppState>();
    // L'ancienne écoute est arrêtée avant d'en ouvrir une nouvelle : deux
    // sessions concurrentes se disputeraient le micro.
    *state.voice.lock().unwrap() = None;

    let mut status = HotkeyStatus {
        combination: phrase.to_string(),
        registered: false,
        error: None,
    };

    if phrase.trim().is_empty() {
        *state.voice_status.lock().unwrap() = status.clone();
        return status;
    }

    let handle = app.clone();
    match smartclip_engine::voice::VoiceTrigger::start(phrase, move || {
        save_from_shortcut(&handle);
    }) {
        Ok(trigger) => {
            *state.voice.lock().unwrap() = Some(trigger);
            status.registered = true;
        }
        Err(e) => status.error = Some(format!("{e:#}")),
    }

    match &status.error {
        Some(error) => tracing::warn!("écoute vocale indisponible : {error}"),
        None => tracing::info!("écoute vocale active"),
    }
    *state.voice_status.lock().unwrap() = status.clone();
    status
}

#[tauri::command]
fn voice_status(state: State<'_, AppState>) -> HotkeyStatus {
    state.voice_status.lock().unwrap().clone()
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            recorder: Mutex::new(None),
            settings: Mutex::new(Settings::load()),
            hotkey: Mutex::new(HotkeyStatus::default()),
            voice: Mutex::new(None),
            voice_status: Mutex::new(HotkeyStatus::default()),
        }
    }
}

/// (Ré)enregistre le raccourci global et mémorise le résultat.
///
/// L'échec était auparavant journalisé dans une console absente en release :
/// le raccourci — fonction principale du produit — pouvait ne jamais marcher
/// sans que rien ne l'indique.
fn apply_hotkey(app: &AppHandle, combination: &str) -> HotkeyStatus {
    let shortcut: Option<Shortcut> = combination.parse().ok();
    let manager = app.global_shortcut();
    let _ = manager.unregister_all();

    let mut status = HotkeyStatus {
        combination: combination.to_string(),
        registered: false,
        error: None,
    };

    match shortcut {
        None => status.error = Some(format!("combinaison « {combination} » non reconnue")),
        Some(shortcut) => match manager.register(shortcut) {
            Ok(()) => status.registered = true,
            Err(e) => {
                status.error = Some(format!(
                    "« {combination} » est refusé par Windows, probablement déjà utilisé par une autre application ({e})"
                ))
            }
        },
    }

    if let Some(error) = &status.error {
        tracing::warn!("raccourci indisponible : {error}");
    } else {
        tracing::info!("raccourci actif : {combination}");
    }
    *app.state::<AppState>().hotkey.lock().unwrap() = status.clone();
    status
}

#[tauri::command]
fn hotkey_status(state: State<'_, AppState>) -> HotkeyStatus {
    state.hotkey.lock().unwrap().clone()
}

/// Un emplacement vacant porte un libellé de la forme `(libre 3)`.
fn is_free_slot(label: &str) -> bool {
    label.starts_with("(libre ")
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn set_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    // Le dossier est créé tout de suite : sans cela l'erreur n'apparaîtrait
    // qu'à la première sauvegarde, longtemps après la saisie fautive.
    std::fs::create_dir_all(&settings.output_dir)
        .map_err(|e| format!("dossier inutilisable : {e}"))?;
    settings.persist().map_err(|e| format!("{e:#}"))?;
    let combination = settings.hotkey.clone();
    let phrase = settings.voice_phrase.clone();
    *state.settings.lock().unwrap() = settings;
    // Raccourci et phrase prennent effet immédiatement : les laisser attendre
    // un redémarrage donnerait l'impression qu'ils ne marchent pas.
    apply_hotkey(&app, &combination);
    apply_voice(&app, &phrase);
    Ok(())
}

#[tauri::command]
fn list_clips(state: State<'_, AppState>) -> Result<Vec<ClipView>, String> {
    let dir = PathBuf::from(&state.settings.lock().unwrap().output_dir);
    let clips = library::scan(&dir).map_err(|e| e.to_string())?;
    Ok(clips
        .into_iter()
        .map(|clip| ClipView {
            path: clip.path.to_string_lossy().into_owned(),
            name: clip.name,
            seconds: clip.seconds,
            megabytes: clip.bytes as f64 / 1_048_576.0,
            created: clip.created,
            tracks: clip
                .tracks
                .into_iter()
                .enumerate()
                .filter(|(_, label)| !is_free_slot(label))
                .map(|(index, label)| TrackView { index, label })
                .collect(),
        })
        .collect())
}

#[tauri::command]
fn delete_clip(path: String) -> Result<(), String> {
    library::delete(&PathBuf::from(path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn is_recording(state: State<'_, AppState>) -> bool {
    state.recorder.lock().unwrap().is_some()
}

#[tauri::command]
fn start_recording(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let mut guard = state.recorder.lock().unwrap();
    if let Some(recorder) = guard.as_ref() {
        return Ok(recorder.tracks());
    }
    let settings = state.settings.lock().unwrap().clone();
    let config = Config {
        buffer_seconds: settings.buffer_seconds,
        ..Config::default()
    };
    let recorder = Recorder::start(config).map_err(|e| format!("{e:#}"))?;
    let tracks = recorder.tracks();
    *guard = Some(recorder);
    Ok(tracks)
}

#[tauri::command]
fn stop_recording(state: State<'_, AppState>) {
    // Le `Drop` du moteur ferme la capture et purge le dossier de travail.
    state.recorder.lock().unwrap().take();
}

#[derive(Serialize)]
struct HealthView {
    running: bool,
    write_errors: u64,
    failure: Option<String>,
    restarts: u32,
    /// Vrai quand la boucle ne progresse plus : le buffer paraît actif mais
    /// n'enregistre rien.
    stalled: bool,
    stalled_ms: u64,
}

/// État du buffer, que la vue interroge périodiquement.
///
/// Un buffer arrêté sans que l'utilisateur le sache est le pire défaut possible
/// pour ce produit : il jouerait des heures en croyant être couvert.
#[tauri::command]
fn recorder_health(state: State<'_, AppState>) -> Option<HealthView> {
    state.recorder.lock().unwrap().as_ref().map(|recorder| {
        let health = recorder.health();
        // Calculé avant de démembrer la structure : `stalled()` emprunte
        // `failure`, qui est déplacé juste après.
        let stalled = health.stalled();
        HealthView {
            running: health.running,
            write_errors: health.write_errors,
            failure: health.failure,
            restarts: health.restarts,
            stalled,
            stalled_ms: health.stalled_ms,
        }
    })
}

#[tauri::command]
fn current_tracks(state: State<'_, AppState>) -> Vec<String> {
    state
        .recorder
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| r.tracks())
        .unwrap_or_default()
}

#[derive(Serialize, Clone)]
struct SaveView {
    path: String,
    seconds: f64,
    total_ms: f64,
}

#[tauri::command]
fn save_clip(state: State<'_, AppState>) -> Result<SaveView, String> {
    let guard = state.recorder.lock().unwrap();
    let recorder = guard.as_ref().ok_or("le buffer n'est pas démarré")?;

    let dir = PathBuf::from(&state.settings.lock().unwrap().output_dir);
    let name = format!("clip_{}.mp4", library::now_seconds());
    let outcome = recorder.save(dir.join(name)).map_err(|e| format!("{e:#}"))?;

    Ok(SaveView {
        path: outcome.path.to_string_lossy().into_owned(),
        seconds: outcome.seconds,
        total_ms: outcome.total_ms(),
    })
}

#[derive(Serialize)]
struct ExportView {
    path: String,
    seconds: f64,
    megabytes: f64,
    peak: f32,
    clipped: bool,
}

/// Applique les gains et écrit le clip final.
///
/// `gains` est indexé par piste **du fichier** : la vue renvoie les indices
/// qu'elle a reçus, jamais des positions d'affichage. Les pistes absentes de la
/// table sont muettes, ce qui coupe naturellement les emplacements vacants.
#[tauri::command]
fn export_mix(
    source: String,
    gains: Vec<(usize, f32)>,
    output: Option<String>,
) -> Result<ExportView, String> {
    let source = PathBuf::from(source);
    let info = smartclip_engine::export::inspect(&source).map_err(|e| format!("{e:#}"))?;

    let mut table = vec![0.0f32; info.audio_streams.len()];
    for (index, gain) in gains {
        if let Some(slot) = table.get_mut(index) {
            *slot = gain;
        }
    }

    let output = output.map(PathBuf::from).unwrap_or_else(|| {
        source.with_file_name(format!(
            "{}_mix.mp4",
            source
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "clip".into())
        ))
    });

    let outcome = smartclip_engine::mix_and_export(&source, &output, &table)
        .map_err(|e| format!("{e:#}"))?;

    Ok(ExportView {
        path: output.to_string_lossy().into_owned(),
        seconds: outcome.seconds,
        megabytes: outcome.bytes as f64 / 1_048_576.0,
        peak: outcome.peak,
        clipped: outcome.clipped(),
    })
}

#[derive(Serialize)]
struct PreviewTrack {
    index: usize,
    label: String,
    /// Chemin du WAV extrait, que la vue convertit en URL `asset:`.
    path: String,
}

/// Sort les pistes du conteneur pour permettre l'écoute en direct.
///
/// Extraire prend le temps de décoder l'audio du clip, d'où l'appel séparé de
/// l'ouverture de l'éditeur : la vue affiche le clip immédiatement et n'active
/// l'écoute que lorsque les pistes sont prêtes.
#[tauri::command]
fn prepare_preview(source: String) -> Result<Vec<PreviewTrack>, String> {
    let source = PathBuf::from(&source);
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".into());
    let directory = std::env::temp_dir().join("smartclip_preview").join(&stem);

    let files =
        smartclip_engine::export::extract_tracks(&source, &directory).map_err(|e| format!("{e:#}"))?;

    // Les libellés viennent du sidecar : le WAV, lui, n'a pas de nom de piste.
    let labels = library::describe(&source)
        .map(|clip| clip.tracks)
        .unwrap_or_default();

    Ok(files
        .into_iter()
        .enumerate()
        .filter(|(index, _)| {
            labels
                .get(*index)
                .map(|label| !is_free_slot(label))
                .unwrap_or(true)
        })
        .map(|(index, path)| PreviewTrack {
            index,
            label: labels
                .get(index)
                .cloned()
                .unwrap_or_else(|| format!("Piste {}", index + 1)),
            path: path.to_string_lossy().into_owned(),
        })
        .collect())
}

/// Efface les WAV de prévisualisation d'un clip.
#[tauri::command]
fn clear_preview(source: String) {
    let stem = PathBuf::from(&source)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        return;
    }
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("smartclip_preview").join(stem));
}

/// Ouvre un chemin dans l'explorateur Windows.
#[tauri::command]
fn reveal(path: String) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg("/select,")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Sauvegarde déclenchée par le raccourci global.
///
/// Elle ne passe pas par la vue : l'application est faite pour rester en fond
/// pendant qu'on joue, fenêtre fermée. Le résultat est ensuite annoncé à la vue
/// par un événement, pour qu'elle rafraîchisse sa bibliothèque si elle est
/// ouverte.
fn save_from_shortcut(app: &AppHandle) {
    let state = app.state::<AppState>();
    let guard = state.recorder.lock().unwrap();
    let Some(recorder) = guard.as_ref() else {
        tracing::warn!("raccourci ignoré : le buffer n'est pas démarré");
        notify_kind(
            app,
            "Aucun clip enregistré",
            "Le buffer n'est pas démarré",
            true,
        );
        let _ = app.emit("save-failed", "Le buffer n'est pas démarré");
        return;
    };

    let dir = PathBuf::from(&state.settings.lock().unwrap().output_dir);
    let name = format!("clip_{}.mp4", library::now_seconds());
    match recorder.save(dir.join(name)) {
        Ok(outcome) => {
            tracing::info!(
                "clip sauvegardé au raccourci : {} ({:.0} ms)",
                outcome.path.display(),
                outcome.total_ms()
            );
            notify(
                app,
                "Clip enregistré",
                &format!(
                    "{} secondes sauvegardées",
                    outcome.seconds.round() as i64
                ),
            );
            let _ = app.emit(
                "clip-saved",
                SaveView {
                    path: outcome.path.to_string_lossy().into_owned(),
                    seconds: outcome.seconds,
                    total_ms: outcome.total_ms(),
                },
            );
        }
        Err(e) => {
            tracing::error!("sauvegarde au raccourci : {e:#}");
            notify_kind(app, "Échec de la sauvegarde", &format!("{e}"), true);
            let _ = app.emit("save-failed", format!("{e:#}"));
        }
    }
}

/// Notification système.
///
/// Le raccourci sert pendant une partie, souvent en plein écran : sans retour
/// visible hors de l'application, rien ne distingue un clip enregistré d'un
/// raccourci ignoré. L'échec d'affichage n'est pas remonté — une notification
/// perdue ne doit pas faire échouer une sauvegarde réussie.
fn notify(app: &AppHandle, title: &str, body: &str) {
    notify_kind(app, title, body, false);
}

/// Signale un événement à l'utilisateur, y compris pendant une partie.
///
/// Deux canaux, parce qu'aucun ne suffit seul :
///
/// - **Un overlay maison**, fenêtre sans décor toujours au premier plan. Les
///   notifications de Windows sont supprimées ou reléguées derrière le jeu dès
///   que celui-ci occupe l'écran — le retour le plus important du produit
///   passait donc inaperçu au moment précis où il compte.
/// - **La notification système**, qui subsiste dans le centre de notifications
///   et permet de retrouver après coup ce qu'on a manqué.
///
/// Limite assumée : en plein écran **exclusif**, aucune fenêtre ne peut se
/// superposer au jeu. Y parvenir exigerait d'injecter un overlay dans son
/// moteur de rendu — précisément ce que le projet s'interdit pour ne pas
/// éveiller les anti-cheat. Les jeux en plein écran fenêtré, largement
/// majoritaires aujourd'hui, ne sont pas concernés.
fn notify_kind(app: &AppHandle, title: &str, body: &str, error: bool) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        tracing::debug!("notification système non affichée : {e}");
    }
    flash_overlay(app, title, body, error);
}

/// Durée d'affichage de la pastille : assez pour être lue d'un coup d'œil,
/// assez court pour ne pas gêner une partie.
const OVERLAY_MS: u64 = 2_600;

fn flash_overlay(app: &AppHandle, title: &str, body: &str, error: bool) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };

    // Coin supérieur droit de l'écran principal, avec une marge.
    if let Ok(Some(monitor)) = window.primary_monitor() {
        let screen = monitor.size();
        let scale = monitor.scale_factor();
        if let Ok(size) = window.outer_size() {
            let x = screen.width as i32 - size.width as i32 - (24.0 * scale) as i32;
            let y = (24.0 * scale) as i32;
            let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
        }
    }

    let _ = window.emit(
        "overlay-flash",
        serde_json::json!({ "title": title, "body": body, "error": error }),
    );
    // `show_without_focus` n'existe pas : on montre la fenêtre, qui est
    // déclarée non focusable, pour ne jamais arracher le clavier au jeu.
    let _ = window.show();

    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(OVERLAY_MS));
        if let Some(window) = handle.get_webview_window("overlay") {
            // L'animation de sortie joue avant le masquage effectif.
            let _ = window.emit("overlay-hide", ());
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = window.hide();
        }
    });
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    // Un seul raccourci est enregistré à la fois : inutile de
                    // comparer la combinaison, elle vient forcément de nous.
                    // Le filtre sur `Pressed` reste indispensable, sans quoi
                    // relâcher la touche déclencherait une seconde sauvegarde.
                    if event.state() == ShortcutState::Pressed {
                        save_from_shortcut(app);
                    }
                })
                .build(),
        )
        .manage(AppState::default())
        .setup(move |app| {
            // Media Foundation est démarré pour toute la durée de vie de
            // l'application, et jamais refermé.
            //
            // Il n'était initialisé que par le thread du moteur : ouvrir un
            // clip sans avoir démarré le buffer — ou après l'avoir arrêté —
            // échouait donc avec MF_E_SHUTDOWN (0xC00D3E85). L'écoute en direct
            // et l'export étaient inutilisables, sans que rien n'explique
            // pourquoi. `MFStartup` étant compté par référence, celui du moteur
            // s'empile sans risque : le compteur ne retombe jamais à zéro tant
            // que la fenêtre vit.
            unsafe {
                MFStartup(MF_VERSION, MFSTARTUP_FULL)
                    .map_err(|e| format!("Media Foundation indisponible : {e}"))?;
            }

            // Le dossier de sortie doit exister avant le premier balayage,
            // sinon la bibliothèque s'ouvre sur une erreur au lieu d'un état
            // vide.
            let state = app.state::<AppState>();
            let dir = PathBuf::from(&state.settings.lock().unwrap().output_dir);
            let _ = std::fs::create_dir_all(dir);

            let (combination, phrase) = {
                let settings = state.settings.lock().unwrap();
                (settings.hotkey.clone(), settings.voice_phrase.clone())
            };
            apply_hotkey(app.handle(), &combination);
            apply_voice(app.handle(), &phrase);

            let open = MenuItem::with_id(app, "open", "Ouvrir SmartClip", true, None::<&str>)?;
            let save = MenuItem::with_id(app, "save", "Sauvegarder un clip", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &save, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("SmartClip Studio")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_window(app),
                    "save" => save_from_shortcut(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Fermer la fenêtre met l'application en fond au lieu de la quitter :
            // le buffer doit continuer à tourner pendant qu'on joue. On quitte
            // par le menu de la barre système.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            list_clips,
            delete_clip,
            is_recording,
            start_recording,
            stop_recording,
            current_tracks,
            recorder_health,
            hotkey_status,
            voice_status,
            save_clip,
            export_mix,
            prepare_preview,
            clear_preview,
            reveal,
        ])
        .run(tauri::generate_context!())
        .expect("le lancement de l'interface a échoué");
}
