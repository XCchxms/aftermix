//! Orchestration : buffer permanent et sauvegarde à la demande.
//!
//! Le moteur tourne sur son propre thread — la capture D3D et le SinkWriter ne
//! sont pas déplaçables — et se pilote par messages. [`Recorder`] est la façade
//! que l'interface manipulera.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use aftermix_core::clock::{HNS_PER_SEC, MasterClock};

use crate::audio::{self, AudioChunk, Source};
use crate::concat;
use crate::segment::{Segment, SegmentFactory, SegmentInfo, SegmentRing};
use crate::video::Capture;
use crate::{CHANNELS, Config, SAMPLE_RATE};

/// État de santé du buffer.
///
/// Le pire scénario du produit est un buffer qui a cessé d'enregistrer sans que
/// l'utilisateur le sache : il joue, vit son grand moment, appuie sur le
/// raccourci — et n'a rien. L'état est donc partagé pour que l'interface puisse
/// alerter au lieu de laisser croire que tout va bien.
#[derive(Debug, Clone)]
pub struct Health {
    pub running: bool,
    /// Écritures ayant échoué depuis le démarrage. Non nul sans être fatal :
    /// quelques frames perdues ne justifient pas d'arrêter la capture.
    pub write_errors: u64,
    /// Renseigné quand la capture s'est arrêtée d'elle-même.
    pub failure: Option<String>,
    /// Redémarrages automatiques, typiquement sur changement de définition.
    /// Chacun vide le buffer : l'interface doit pouvoir le dire.
    pub restarts: u32,
    /// Images volontairement sautées pour soulager l'encodeur.
    ///
    /// Un nombre non nul n'est pas une anomalie : c'est la régulation qui
    /// travaille. Il monte quand le GPU est pris par un jeu exigeant.
    pub skipped_frames: u64,
    /// Millisecondes écoulées depuis la dernière image traitée.
    ///
    /// C'est le seul indicateur capable de révéler un **blocage** : quand
    /// `WriteSample` n'a pas rendu la main, aucun compteur d'erreur ne bouge et
    /// tout paraît normal. Une campagne s'est ainsi figée deux heures, GPU
    /// saturé par un jeu, sans qu'aucun voyant ne s'allume.
    pub stalled_ms: u64,
}

/// Au-delà de ce délai sans image traitée, le moteur est considéré comme figé.
///
/// Généreux à dessein : une sauvegarde légitime immobilise la boucle plus d'une
/// seconde, et un pic de charge peut la ralentir sans qu'elle soit bloquée.
const STALL_THRESHOLD_MS: u64 = 15_000;

/// Pourquoi la boucle de capture a rendu la main.
enum RunOutcome {
    /// Arrêt demandé.
    Stopped,
    /// Le moteur doit repartir sur une nouvelle configuration d'écran.
    Restart(String),
}

impl Default for Health {
    fn default() -> Self {
        Self {
            running: true,
            write_errors: 0,
            failure: None,
            restarts: 0,
            skipped_frames: 0,
            stalled_ms: 0,
        }
    }
}

impl Health {
    /// Vrai quand la boucle n'a plus traité d'image depuis trop longtemps.
    pub fn stalled(&self) -> bool {
        self.running && self.stalled_ms > STALL_THRESHOLD_MS
    }
}

/// Horodatage partagé de la dernière image traitée, en millisecondes depuis le
/// démarrage du moteur.
struct Heartbeat {
    origin: Instant,
    last_ms: std::sync::atomic::AtomicU64,
}

impl Heartbeat {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            last_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn beat(&self) {
        self.last_ms.store(
            self.origin.elapsed().as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    fn silence_ms(&self) -> u64 {
        (self.origin.elapsed().as_millis() as u64)
            .saturating_sub(self.last_ms.load(Ordering::Relaxed))
    }
}

/// Nombre d'échecs consécutifs au-delà duquel on cesse d'insister.
///
/// À 60 images par seconde, cela représente deux secondes : assez pour absorber
/// un incident passager, trop court pour laisser durer une panne réelle.
const MAX_CONSECUTIVE_ERRORS: u32 = 120;

/// Ce qu'une sauvegarde a produit.
#[derive(Debug, Clone)]
pub struct SaveOutcome {
    pub path: PathBuf,
    pub seconds: f64,
    pub bytes: u64,
    /// Temps de finalisation du segment en cours.
    pub flush_ms: f64,
    /// Temps de recollage.
    pub concat_ms: f64,
    pub tracks: Vec<String>,
}

impl SaveOutcome {
    pub fn total_ms(&self) -> f64 {
        self.flush_ms + self.concat_ms
    }
}

enum Command {
    Save {
        path: PathBuf,
        reply: Sender<Result<SaveOutcome>>,
    },
    Tracks(Sender<Vec<String>>),
    Stop,
}

/// Une application affectée à un emplacement de piste.
struct Slot {
    label: String,
    /// Exécutable occupant l'emplacement. C'est lui, et non le PID, qui sert à
    /// reconnaître une application : plusieurs processus de même nom rendent du
    /// son et le loopback les capte tous ensemble.
    process: String,
    pid: Option<u32>,
    /// Drapeau d'arrêt propre à cette source : on peut la couper sans toucher
    /// aux autres, ce qu'exige la libération d'un emplacement.
    stop: Arc<AtomicBool>,
    /// Faux dès que le thread de capture a rendu la main, pour quelque raison
    /// que ce soit. Un emplacement mort doit repasser au silence.
    alive: Arc<AtomicBool>,
}

/// Table des emplacements. L'indice est le numéro de piste dans le fichier.
struct SlotMap {
    slots: Vec<Option<Slot>>,
}

impl SlotMap {
    fn new(size: usize) -> Self {
        Self {
            slots: (0..size).map(|_| None).collect(),
        }
    }

    fn labels(&self) -> Vec<String> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                slot.as_ref()
                    .map(|s| s.label.clone())
                    .unwrap_or_else(|| format!("(libre {i})"))
            })
            .collect()
    }

    fn holds(&self, process: &str) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| slot.process.eq_ignore_ascii_case(process))
    }

    fn free_index(&self) -> Option<usize> {
        self.slots.iter().position(|slot| slot.is_none())
    }

    /// Un emplacement n'est actif que si sa capture tourne encore : dès qu'elle
    /// s'arrête, le moteur doit reprendre l'alimentation en silence.
    fn is_active(&self, index: usize) -> bool {
        self.slots
            .get(index)
            .and_then(|slot| slot.as_ref())
            .map(|slot| slot.alive.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Libère les emplacements dont la capture a rendu la main.
    ///
    /// Rend les libellés concernés, pour pouvoir les journaliser — une source
    /// qui tombe seule est un incident que l'utilisateur doit pouvoir relier à
    /// une piste muette dans ses clips.
    fn reap(&mut self) -> Vec<String> {
        let mut lost = Vec::new();
        for slot in self.slots.iter_mut() {
            let dead = slot
                .as_ref()
                .map(|s| !s.alive.load(Ordering::Relaxed))
                .unwrap_or(false);
            if dead {
                if let Some(slot) = slot.take() {
                    lost.push(slot.label);
                }
            }
        }
        lost
    }
}

/// Façade du moteur. Le buffer tourne tant que cette valeur vit.
pub struct Recorder {
    commands: Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
    sources: Vec<String>,
    health: Arc<std::sync::Mutex<Health>>,
    heartbeat: Arc<Heartbeat>,
}

impl Recorder {
    /// Démarre le buffer permanent.
    ///
    /// Bloque jusqu'à ce que la capture soit effectivement en route, de façon
    /// qu'un `save` immédiatement consécutif trouve déjà des données.
    pub fn start(config: Config) -> Result<Self> {
        let (commands, command_rx) = channel();
        let (ready_tx, ready_rx) = channel::<Result<Vec<String>>>();
        let health = Arc::new(std::sync::Mutex::new(Health::default()));
        let heartbeat = Arc::new(Heartbeat::new());

        let thread = {
            let (health, heartbeat) = (Arc::clone(&health), Arc::clone(&heartbeat));
            std::thread::Builder::new()
                .name("aftermix-engine".into())
                .spawn(move || engine_thread(config, command_rx, ready_tx, health, heartbeat))?
        };

        let sources = ready_rx
            .recv()
            .context("le moteur s'est arrêté avant d'être prêt")??;

        Ok(Self {
            commands,
            thread: Some(thread),
            sources,
            health,
            heartbeat,
        })
    }

    /// État courant du buffer, à interroger régulièrement par l'interface.
    pub fn health(&self) -> Health {
        let mut health = self.health.lock().unwrap().clone();
        if health.running {
            health.stalled_ms = self.heartbeat.silence_ms();
        }
        health
    }

    /// Libellés des pistes au démarrage, dans l'ordre du fichier.
    pub fn initial_tracks(&self) -> &[String] {
        &self.sources
    }

    /// Libellés des pistes maintenant.
    ///
    /// Ils changent en cours de session : une application lancée après le
    /// démarrage prend un emplacement libre au segment suivant.
    pub fn tracks(&self) -> Vec<String> {
        let (reply, answer) = channel();
        if self.commands.send(Command::Tracks(reply)).is_err() {
            return self.sources.clone();
        }
        answer.recv().unwrap_or_else(|_| self.sources.clone())
    }

    /// Fige le contenu du buffer dans `path`.
    pub fn save(&self, path: impl Into<PathBuf>) -> Result<SaveOutcome> {
        let (reply, answer) = channel();
        self.commands
            .send(Command::Save {
                path: path.into(),
                reply,
            })
            .context("le moteur ne répond plus")?;
        // Attente bornée : une sauvegarde dépasse rarement quelques secondes, et
        // l'appelant est souvent l'interface. La laisser attendre sans limite
        // figeait toute l'application quand le moteur était bloqué.
        answer
            .recv_timeout(Duration::from_secs(60))
            .context("le moteur n'a pas répondu en 60 s (buffer probablement bloqué)")?
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        let Some(thread) = self.thread.take() else {
            return;
        };

        // Un moteur figé n'est pas attendu : il est abandonné.
        //
        // Le thread est bloqué dans un appel COM dont rien ne le sortira, et
        // `join` ne rendrait jamais la main — l'application entière se figerait
        // en tentant de s'arrêter, ou de redémarrer l'enregistrement. Le thread
        // reste alors zombie jusqu'à la fin du processus : c'est une fuite
        // assumée, et de loin le moindre mal.
        if self.health().stalled() {
            tracing::warn!("moteur figé : thread abandonné sans attendre");
            return;
        }
        let _ = thread.join();
    }
}

fn engine_thread(
    config: Config,
    commands: Receiver<Command>,
    ready: Sender<Result<Vec<String>>>,
    health: Arc<std::sync::Mutex<Health>>,
    heartbeat: Arc<Heartbeat>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            let _ = ready.send(Err(e.into()));
            return;
        }
    }

    // Le moteur se relance de lui-même quand la définition de l'écran change :
    // un jeu qui passe en plein écran ne doit pas laisser l'utilisateur avec un
    // buffer mort. Le contenu déjà tamponné est perdu — il est à l'ancienne
    // définition et ne peut pas être recollé avec la nouvelle — mais la
    // capture, elle, reprend.
    let mut announced = false;
    let mut recent_restarts = 0u32;
    let mut last_restart: Option<Instant> = None;

    let outcome = loop {
        match run(&config, &commands, &ready, &health, &heartbeat, &mut announced) {
            Ok(RunOutcome::Stopped) => break Ok(()),
            Ok(RunOutcome::Restart(reason)) => {
                // Un redémarrage isolé est normal ; une rafale signale une
                // cause qui ne se résoudra pas d'elle-même — GPU réellement
                // défaillant, pilote instable. Insister ferait tourner une
                // boucle inutile en consommant la machine.
                let rapid = last_restart
                    .map(|t| t.elapsed() < Duration::from_secs(60))
                    .unwrap_or(false);
                recent_restarts = if rapid { recent_restarts + 1 } else { 1 };
                last_restart = Some(Instant::now());

                if recent_restarts > 5 {
                    break Err(anyhow::anyhow!(
                        "{recent_restarts} redémarrages en moins d'une minute, dernier motif : {reason}"
                    ));
                }

                tracing::info!("redémarrage du moteur : {reason}");
                health.lock().unwrap().restarts += 1;
                // Laisse au pilote le temps de se rétablir avant de tout
                // reconstruire ; sans cette pause, la reconstruction échoue
                // souvent et déclenche un nouveau tour.
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            Err(e) => break Err(e),
        }
    };

    match outcome {
        Ok(()) => health.lock().unwrap().running = false,
        Err(e) => {
            // Le moteur s'est arrêté seul : l'interface doit pouvoir le dire à
            // l'utilisateur, faute de quoi il croira enregistrer dans le vide.
            tracing::error!("le moteur s'est arrêté : {e:#}");
            let mut health = health.lock().unwrap();
            health.running = false;
            health.failure = Some(format!("{e:#}"));
            // Si l'échec précède le signal de démarrage, `start` l'apprendra
            // par ce canal ; sinon il n'y a plus personne pour l'écouter.
            let _ = ready.send(Err(e));
        }
    }

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
}

fn run(
    config: &Config,
    commands: &Receiver<Command>,
    ready: &Sender<Result<Vec<String>>>,
    health: &Arc<std::sync::Mutex<Health>>,
    heartbeat: &Arc<Heartbeat>,
    announced: &mut bool,
) -> Result<RunOutcome> {
    let _ = std::fs::remove_dir_all(&config.workdir);
    std::fs::create_dir_all(&config.workdir)?;

    let mut capture = Capture::primary_monitor()?;

    let factory = SegmentFactory::new(
        &capture.device,
        capture.width,
        capture.height,
        config.fps,
        config.bitrate,
        config.track_slots,
    )?;

    let clock = MasterClock::new();
    // Un canal borné **par piste**, jamais un canal partagé.
    //
    // Avec une file commune, la source la plus bavarde rafle tous les
    // emplacements dès que le muxeur ralentit : le micro, en capture WASAPI
    // classique, produit bien plus de paquets que les loopbacks et leur faisait
    // perdre la quasi-totalité de leur contenu — 7 001 paquets écartés sur la
    // seule piste du micro lors d'une reproduction. Le clip final ne contenait
    // plus que la voix, les autres pistes réduites au silence.
    //
    // Isolées, les pistes ne se concurrencent plus : une source qui déborde
    // n'ampute qu'elle-même. ~4 s de réserve chacune.
    let (audio_senders, audio_receivers): (Vec<SyncSender<AudioChunk>>, Vec<Receiver<AudioChunk>>) =
        (0..config.track_slots).map(|_| sync_channel(400)).unzip();
    let mut slots = SlotMap::new(config.track_slots);

    // Le micro occupe le dernier emplacement : il est toujours présent, et le
    // réserver en fin de table laisse les premiers aux applications.
    //
    // L'emplacement reste réservé même micro coupé. Le libérer aux applications
    // changerait la structure des flux d'une session à l'autre, et rebrancher un
    // micro exigerait de vider le buffer.
    let mic_index = config.track_slots - 1;
    if config.capture_microphone {
        attach(
            &mut slots,
            mic_index,
            Source {
                label: "Micro".to_string(),
                process: String::new(),
                pid: None,
                device: config.microphone.clone(),
            },
            &clock,
            &audio_senders,
        );
    }

    for source in audio::discover(config.max_sources)? {
        let Some(index) = slots.free_index().filter(|i| *i != mic_index) else {
            break;
        };
        attach(&mut slots, index, source, &clock, &audio_senders);
    }

    // ── rotation hors du chemin critique ──
    //
    // Ouvrir et finaliser un segment coûtent des centaines de millisecondes :
    // dans la boucle, cela figeait l'image un tiers du temps. Un thread
    // pré-ouvre le segment suivant, un autre finalise le précédent, et la
    // rotation se réduit à un échange de pointeur (0,3 ms mesuré).
    let (ready_seg_tx, ready_seg_rx) = sync_channel::<Segment>(1);
    let (close_tx, close_rx) = channel::<Segment>();
    let (closed_tx, closed_rx) = channel::<SegmentInfo>();

    let opener = spawn_opener(factory, config.workdir.clone(), ready_seg_tx);
    let closer = spawn_closer(close_rx, closed_tx);

    let mut segment = ready_seg_rx.recv().context("aucun segment initial")?;
    let mut ring = SegmentRing::new(config.buffer_seconds, config.max_bytes);

    // Veille des applications qui apparaissent ou disparaissent en cours de
    // session. Un simple rafraîchissement périodique suffit : rater trois
    // secondes de Discord au lancement est sans conséquence, et cela évite de
    // s'abonner aux notifications de sessions audio, autrement plus lourdes.
    let (watch_tx, watch_rx) = channel::<Vec<Source>>();
    let watch_stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (stop, max_sources) = (Arc::clone(&watch_stop), config.max_sources);
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(3));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(sources) = audio::discover(max_sources) {
                    if watch_tx.send(sources).is_err() {
                        return;
                    }
                }
            }
        })
    };

    capture.start()?;
    // Une seule fois : au redémarrage, plus personne n'écoute ce canal.
    if !*announced {
        let _ = ready.send(Ok(slots.labels()));
        *announced = true;
    }

    // Bloc de silence pour les emplacements vacants. Sans lui, un flux déclaré
    // mais jamais alimenté empêche la finalisation du segment.
    //
    // Il couvre 100 ms, et non une frame. Écrit à la cadence vidéo, le silence
    // produisait 60 paquets par seconde et par piste : le conteneur se
    // fragmentait au point de faire passer un export de 0,7 s à 17 s. Cent
    // millisecondes correspondent à ce qu'écrivent les vraies pistes.
    const SILENCE_MS: u32 = 100;
    let silence_frames = SAMPLE_RATE * SILENCE_MS / 1000;
    let silence = vec![0i16; silence_frames as usize * CHANNELS as usize];
    let silence_period = (config.fps * SILENCE_MS / 1000).max(1) as u64;

    let frame_duration = HNS_PER_SEC / config.fps as i64;
    let tick = Duration::from_nanos(1_000_000_000 / config.fps as u64);
    let frames_per_segment = (config.segment_seconds * config.fps as f64).round() as u64;

    let mut next_tick = Instant::now();
    let mut index = 0u64;
    let mut consecutive_errors = 0u32;
    let mut run_outcome = RunOutcome::Stopped;
    /// Rotations à attendre avant de retenter le micro, soit ~16 s.
    const MIC_RETRY_ROTATIONS: u32 = 8;
    let mut mic_retry = 0u32;
    let mut segment_first_frame = 0u64;

    loop {
        next_tick += tick;

        // Les commandes sont lues en tête de tour : un arrêt ou une sauvegarde
        // ne peut jamais attendre derrière une rotation de segment.
        match commands.try_recv() {
            Ok(Command::Save { path, reply }) => {
                let outcome = save_now(
                    &mut segment,
                    &mut ring,
                    &ready_seg_rx,
                    &closed_rx,
                    &path,
                    &slots.labels(),
                );
                let _ = reply.send(outcome);
                segment_first_frame = index;
            }
            Ok(Command::Tracks(reply)) => {
                let _ = reply.send(slots.labels());
            }
            Ok(Command::Stop) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        // Signale que la boucle vit encore. Une image traitée est la seule
        // preuve que `WriteSample` rend la main.
        heartbeat.beat();

        if let Some((texture, _fresh)) = capture.next_texture(index)? {
            // Horodatage vidéo en **cadence constante**, dans le segment.
            //
            // Le dater au QPC paraissait plus juste — c'est l'horloge de
            // l'audio — mais inscrit dans le fichier chaque hoquet de la
            // boucle : un retard de deux secondes devient un trou de deux
            // secondes, pendant lequel le lecteur fige l'image alors que le son
            // continue. Mesuré sur un clip : jusqu'à 24 secondes sans une seule
            // image, et 12 images par seconde en moyenne.
            //
            // En cadence constante, la vidéo reste fluide par construction. Le
            // prix est une dérive lente vis-à-vis de l'audio : mesurée à −33 ms
            // sur 5 minutes au Spike 3, sous le seuil audible de 40 ms, et sans
            // commune mesure avec un trou de plusieurs secondes.
            let pts = (index - segment_first_frame) as i64 * frame_duration;

            // Une écriture qui échoue ne doit pas tuer le buffer : un disque
            // momentanément saturé ou un encodeur qui hoquette se traduit par
            // quelques images perdues, pas par la fin de l'enregistrement.
            let mut outcome = segment.write_video(texture, pts, frame_duration);

            // L'audio suit immédiatement l'image, dans le même tour.
            //
            // Le muxeur MP4 exige un entrelacement : écrire de l'audio sans
            // image le bloque, comme à l'export. Les deux flux avancent donc
            // ensemble.
            for receiver in &audio_receivers {
                for chunk in receiver.try_iter() {
                    outcome = outcome.and(segment.write_audio(chunk.track, &chunk.pcm));
                }
            }

            // Les emplacements vacants reçoivent un bloc de silence toutes les
            // 100 ms. Leur position découle du compteur d'échantillons de la
            // piste, comme pour les vraies sources.
            let since_start = index - segment_first_frame;
            if since_start % silence_period == 0 {
                for slot_index in 0..config.track_slots {
                    if !slots.is_active(slot_index) {
                        outcome = outcome.and(segment.write_audio(slot_index, &silence));
                    }
                }
            }

            match outcome {
                Ok(()) => consecutive_errors = 0,
                Err(e) => {
                    consecutive_errors += 1;
                    let mut state = health.lock().unwrap();
                    state.write_errors += 1;
                    // Une trace par seconde au plus : à 60 images par seconde,
                    // journaliser chaque échec noierait la cause réelle.
                    if consecutive_errors % config.fps.max(1) == 1 {
                        tracing::warn!("écriture en échec ({consecutive_errors} d'affilée) : {e:#}");
                    }
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        bail!("écriture impossible depuis {consecutive_errors} images : {e:#}");
                    }
                }
            }

            if index + 1 - segment_first_frame >= frames_per_segment {
                // La définition est figée dans le flux vidéo du segment : si
                // l'écran change, la seule issue est de repartir. On le
                // contrôle à la frontière de segment, là où le coût est nul.
                // Périphérique perdu : veille, mise à jour de pilote, plantage
                // GPU. Rien n'est réparable en place, mais tout est
                // reconstructible — c'est exactement le même traitement qu'un
                // changement de définition.
                if let Some(reason) = capture.device_lost() {
                    run_outcome =
                        RunOutcome::Restart(format!("périphérique graphique perdu : {reason}"));
                    break;
                }

                if let Some((width, height)) = capture.resolution_change() {
                    run_outcome = RunOutcome::Restart(format!(
                        "définition passée de {}×{} à {width}×{height}",
                        capture.width, capture.height
                    ));
                    break;
                }

                // Les changements de sources ne prennent effet qu'ici : pendant
                // un segment, la table des emplacements reste figée. La
                // transition tombe ainsi sur une frontière de segment, où elle
                // ne peut pas produire de chevauchement d'échantillons.
                for lost in slots.reap() {
                    tracing::warn!("piste perdue : {lost}");
                }
                // Le micro ne réapparaît pas dans le balayage — il n'a pas de
                // PID. Sans cette relance, débrancher un casque le ferait
                // disparaître jusqu'à la fin de la session.
                //
                // Une temporisation est indispensable : sans micro branché,
                // réessayer à chaque rotation lancerait un thread toutes les
                // deux secondes pour rien.
                mic_retry = mic_retry.saturating_sub(1);
                if config.capture_microphone && !slots.is_active(mic_index) && mic_retry == 0 {
                    mic_retry = MIC_RETRY_ROTATIONS;
                    attach(
                        &mut slots,
                        mic_index,
                        Source {
                            label: "Micro".to_string(),
                            process: String::new(),
                            pid: None,
                            device: config.microphone.clone(),
                        },
                        &clock,
                        &audio_senders,
                    );
                }
                if let Some(seen) = watch_rx.try_iter().last() {
                    reconcile(&mut slots, seen, mic_index, &clock, &audio_senders);
                }

                // Attente bornée : si l'ouvreur est lui-même bloqué — création
                // de SinkWriter sur un GPU saturé — un `recv` nu figerait le
                // moteur pour toujours.
                let next = ready_seg_rx
                    .recv_timeout(Duration::from_secs(10))
                    .context("aucun segment disponible depuis 10 s")?;
                let previous = std::mem::replace(&mut segment, next);
                let _ = close_tx.send(previous);
                for info in closed_rx.try_iter() {
                    ring.push(info);
                }
                // Rattrape les fichiers qui ont échappé à l'anneau. Sans ce
                // balayage, ils s'accumulent indéfiniment sur le disque.
                let swept = ring.sweep(&config.workdir);
                if swept > 0 {
                    tracing::debug!("{swept} segment(s) orphelin(s) supprimé(s)");
                }
                segment_first_frame = index + 1;
            }
        }

        // Le compteur avance à chaque tour d'horloge, qu'une image soit arrivée
        // ou non — c'est ainsi que procède le Spike 4, dont les clips sont
        // mesurés parfaitement réguliers.
        //
        // L'attacher aux seules images désolidarisait les horodatages du temps
        // qui passe : la rotation des segments et le repère du silence
        // dérivaient dès qu'une image manquait.
        index += 1;


        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            // Retard accumulé : on repart d'un tick complet à partir de
            // maintenant. Repartir de `now` seul ferait tourner la boucle sans
            // jamais dormir — un cœur saturé pour rien.
            next_tick = now + tick;
        }
    }

    tracing::debug!("arrêt du moteur demandé");
    watch_stop.store(true, Ordering::Relaxed);
    for slot in slots.slots.iter().flatten() {
        slot.stop.store(true, Ordering::Relaxed);
    }

    // Fermer les canaux suffit à faire sortir les threads de service : le
    // closer épuise sa file, l'ouvreur échoue à son prochain envoi. On ne les
    // joint pas — un `Finalize` en cours peut durer, et rien de ce qu'ils font
    // encore n'est nécessaire à un arrêt correct.
    drop(close_tx);
    drop(ready_seg_rx);
    drop(audio_senders);
    let _ = closer;
    let _ = opener;
    let _ = watcher;

    // Purge de tout le dossier, et non du seul anneau : au moment de l'arrêt,
    // des segments sont encore en cours de finalisation et n'y figurent pas
    // encore. Sans cela, quelques fichiers survivent à chaque session.
    ring.clear();
    if let Ok(entries) = std::fs::read_dir(&config.workdir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    capture.close()?;
    tracing::debug!("moteur arrêté");
    Ok(run_outcome)
}

/// Finalise le segment courant puis recolle l'anneau.
///
/// La finalisation est synchrone et c'est délibéré : sans elle, les dernières
/// secondes — celles que l'utilisateur veut garder — ne sont pas lisibles.
fn save_now(
    segment: &mut Segment,
    ring: &mut SegmentRing,
    ready_seg_rx: &Receiver<Segment>,
    closed_rx: &Receiver<SegmentInfo>,
    path: &Path,
    labels: &[String],
) -> Result<SaveOutcome> {
    let flush_start = Instant::now();
    let next = ready_seg_rx
        .recv()
        .context("aucun segment de remplacement disponible")?;
    let current = std::mem::replace(segment, next);
    let info = current.close()?;
    let flush_ms = flush_start.elapsed().as_secs_f64() * 1000.0;

    for pending in closed_rx.try_iter() {
        ring.push(pending);
    }
    ring.push(info);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let concat_start = Instant::now();
    let outcome = concat::concat(ring.segments(), path)?;
    let concat_ms = concat_start.elapsed().as_secs_f64() * 1000.0;

    let seconds = ring.duration_hns() as f64 / HNS_PER_SEC as f64;

    // Le nom des pistes n'existe nulle part dans le MP4 : sans ce sidecar,
    // l'éditeur ne pourrait afficher que « piste 0, piste 1 ». Son écriture ne
    // conditionne pas la sauvegarde — un clip sans métadonnées reste lisible et
    // listable, avec des libellés de repli.
    let meta = crate::library::ClipMeta {
        tracks: labels.to_vec(),
        seconds,
        created: crate::library::now_seconds(),
        // Attribué dès la sauvegarde plutôt qu'au premier partage : un
        // identifiant qui apparaît après coup obligerait à réécrire le sidecar
        // d'un clip qu'on est peut-être en train de lire.
        share_id: Some(crate::library::new_share_id()),
    };
    if let Err(e) = meta.write(path) {
        tracing::warn!("métadonnées non écrites : {e:#}");
    }

    // Vignette extraite une fois pour toutes, à côté du clip. Son échec ne
    // compromet pas la sauvegarde : la bibliothèque sait afficher une carte
    // sans image.
    let thumbnail = crate::library::ClipMeta::thumbnail_path(path);
    if let Err(e) = crate::export::extract_thumbnail(path, &thumbnail) {
        tracing::warn!("vignette non extraite : {e:#}");
    }

    Ok(SaveOutcome {
        path: path.to_path_buf(),
        seconds,
        bytes: outcome.bytes,
        flush_ms,
        concat_ms,
        tracks: labels.to_vec(),
    })
}

/// Affecte une source à un emplacement et lance sa capture.
fn attach(
    slots: &mut SlotMap,
    index: usize,
    source: Source,
    clock: &MasterClock,
    audio_senders: &[SyncSender<AudioChunk>],
) {
    let Some(sender) = audio_senders.get(index) else {
        tracing::error!("emplacement {index} sans canal audio");
        return;
    };
    let stop = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));
    let (clock, tx, pid) = (clock.clone(), sender.clone(), source.pid);
    let device = source.device.clone();
    let (thread_stop, thread_alive) = (Arc::clone(&stop), Arc::clone(&alive));
    // Les threads de capture ne sont pas conservés : ils s'arrêtent sur leur
    // propre drapeau et signalent leur fin par `alive`.
    std::thread::spawn(move || {
        audio::capture_source(index, pid, device, clock, thread_stop, thread_alive, tx)
    });
    tracing::info!("piste {index} : {}", source.label);
    slots.slots[index] = Some(Slot {
        label: source.label,
        process: source.process,
        pid: source.pid,
        stop,
        alive,
    });
}

/// Aligne la table des emplacements sur les applications réellement présentes.
fn reconcile(
    slots: &mut SlotMap,
    seen: Vec<Source>,
    mic_index: usize,
    clock: &MasterClock,
    audio_senders: &[SyncSender<AudioChunk>],
) {
    // Libère les emplacements dont l'application a disparu. Le micro n'a pas de
    // PID et n'est jamais concerné.
    //
    // La comparaison porte sur l'exécutable : un processus qui redémarre change
    // de PID sans que l'application ait disparu, et couper la piste dans ce cas
    // ferait perdre le son pour rien.
    for index in 0..slots.slots.len() {
        let Some(slot) = &slots.slots[index] else {
            continue;
        };
        if slot.pid.is_none() {
            continue;
        }
        if !seen
            .iter()
            .any(|s| s.process.eq_ignore_ascii_case(&slot.process))
        {
            tracing::info!("piste {index} libérée : {} s'est fermé", slot.label);
            slot.stop.store(true, Ordering::Relaxed);
            slots.slots[index] = None;
        }
    }

    // Accueille les nouvelles venues dans les emplacements libres.
    for source in seen {
        if source.pid.is_none() || slots.holds(&source.process) {
            continue;
        }
        let Some(index) = slots.free_index().filter(|i| *i != mic_index) else {
            tracing::debug!("aucun emplacement libre pour {}", source.label);
            break;
        };
        attach(slots, index, source, clock, audio_senders);
    }
}

fn spawn_opener(
    factory: SegmentFactory,
    workdir: PathBuf,
    ready: SyncSender<Segment>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut index = 0usize;
        loop {
            match factory.open(workdir.join(format!("seg{index:06}.mp4"))) {
                // Le canal est borné à un élément : l'envoi bloque tant que le
                // segment d'avance n'a pas été consommé, ce qui suffit à ne
                // jamais en accumuler.
                Ok(segment) => {
                    if ready.send(segment).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::error!("ouverture d'un segment impossible : {e:#}");
                    return;
                }
            }
            index += 1;
        }
    })
}

fn spawn_closer(
    close_rx: Receiver<Segment>,
    closed_tx: Sender<SegmentInfo>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        for segment in close_rx {
            match segment.close() {
                Ok(info) => {
                    if closed_tx.send(info).is_err() {
                        return;
                    }
                }
                Err(e) => tracing::error!("finalisation d'un segment : {e:#}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_moteur_sain_n_est_pas_declare_fige() {
        let health = Health {
            stalled_ms: STALL_THRESHOLD_MS - 1,
            ..Health::default()
        };
        assert!(!health.stalled());
    }

    #[test]
    fn un_moteur_arrete_n_est_pas_fige() {
        // Distinction utile : « arrêté » et « figé » appellent des messages
        // différents dans l'interface.
        let health = Health {
            running: false,
            stalled_ms: STALL_THRESHOLD_MS * 10,
            ..Health::default()
        };
        assert!(!health.stalled());
    }

    #[test]
    fn un_silence_prolonge_est_declare_fige() {
        let health = Health {
            stalled_ms: STALL_THRESHOLD_MS + 1,
            ..Health::default()
        };
        assert!(health.stalled());
    }
}

/// Vérifie qu'une configuration est exploitable avant de démarrer le moteur.
pub fn validate(config: &Config) -> Result<()> {
    if config.buffer_seconds < 5.0 {
        bail!("le buffer doit couvrir au moins 5 secondes");
    }
    if config.segment_seconds <= 0.0 || config.segment_seconds > config.buffer_seconds {
        bail!("la durée de segment doit être positive et tenir dans le buffer");
    }
    if config.fps == 0 {
        bail!("la cadence doit être positive");
    }
    Ok(())
}
