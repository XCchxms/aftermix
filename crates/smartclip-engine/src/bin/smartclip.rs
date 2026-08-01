//! CLI du moteur SmartClip : buffer permanent et sauvegarde au raccourci.
//!
//! C'est l'étape 1 de la V1 — le moteur complet, pilotable, sans interface.
//! L'interface Tauri appellera plus tard le même [`Recorder`].
//!
//! Usage : `cargo run --release --bin smartclip -- --buffer 60`
//! puis **Ctrl+Shift+X** pour sauvegarder, **Ctrl+C** pour quitter.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use smartclip_engine::{Config, Recorder, recorder};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

const HOTKEY_ID: i32 = 1;

struct Args {
    config: Config,
    output: PathBuf,
    /// Laisse tourner le buffer N secondes, sauvegarde une fois, puis quitte.
    /// Permet de vérifier la chaîne complète sans dépendre d'une frappe.
    auto_save: Option<f64>,
    /// Campagne d'endurance : laisse tourner N secondes en relevant mémoire,
    /// erreurs et redémarrages, avec une sauvegarde régulière.
    duration: Option<f64>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut config = Config::default();
        let mut output = dirs_videos().join("SmartClip");
        let mut auto_save = None;
        let mut duration = None;

        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || {
                it.next()
                    .with_context(|| format!("valeur manquante après {flag}"))
            };
            match flag.as_str() {
                "--buffer" => config.buffer_seconds = value()?.parse()?,
                "--max-mb" => config.max_bytes = value()?.parse::<u64>()? * 1_048_576,
                "--segment" => config.segment_seconds = value()?.parse()?,
                "--fps" => config.fps = value()?.parse()?,
                "--bitrate" => config.bitrate = value()?.parse()?,
                "--sources" => config.max_sources = value()?.parse()?,
                "--workdir" => config.workdir = PathBuf::from(value()?),
                "--output" => output = PathBuf::from(value()?),
                "--auto-save" => auto_save = Some(value()?.parse()?),
                "--duration" => duration = Some(value()?.parse()?),
                "--help" | "-h" => {
                    println!(
                        "smartclip — buffer vidéo permanent, audio séparé par application\n\n\
                         Options :\n  \
                         --buffer <s>     durée conservée en arrière (défaut 60)\n  \
                         --max-mb <Mo>    plafond disque (défaut 2048)\n  \
                         --segment <s>    durée d'un segment (défaut 2)\n  \
                         --fps <n>        cadence (défaut 60)\n  \
                         --bitrate <bps>  consigne de débit (défaut 20000000)\n  \
                         --sources <n>    applications capturées séparément (défaut 4)\n  \
                         --output <dir>   dossier des clips\n  \
                         --workdir <dir>  dossier de travail du buffer\n\n\
                         Ctrl+Shift+X sauvegarde, Ctrl+C quitte."
                    );
                    std::process::exit(0);
                }
                other => bail!("option inconnue : {other} (voir --help)"),
            }
        }
        Ok(Self {
            config,
            output,
            auto_save,
            duration,
        })
    }
}

/// Sous-commande `list` : la bibliothèque telle que l'interface l'affichera.
fn run_list(directory: &std::path::Path) -> Result<()> {
    let clips = smartclip_engine::library::scan(directory)?;
    if clips.is_empty() {
        println!("aucun clip dans {}", directory.display());
        return Ok(());
    }
    println!("{} clip(s) dans {}\n", clips.len(), directory.display());
    for clip in &clips {
        println!(
            "{:<28} {:>6.1}s {:>7.1} Mo   {}",
            clip.name,
            clip.seconds,
            clip.bytes as f64 / 1_048_576.0,
            if clip.metadata_missing {
                "(métadonnées absentes)".to_string()
            } else {
                clip.tracks.join(", ")
            }
        );
    }
    Ok(())
}

/// Empreinte mémoire du processus, en mégaoctets.
fn rss_mo() -> u64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe {
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size).is_ok() {
            (counters.WorkingSetSize / 1_048_576) as u64
        } else {
            0
        }
    }
}

/// Campagne d'endurance.
///
/// Le buffer est censé tourner des heures en fond ; jusqu'ici il n'avait jamais
/// été éprouvé au-delà de quelques dizaines de secondes. On relève la mémoire,
/// les erreurs d'écriture et les redémarrages, et l'on sauvegarde
/// périodiquement pour exercer aussi le chemin de finalisation et de recollage.
fn endurance(recorder: &Recorder, seconds: f64, output: &std::path::Path) -> Result<()> {
    use std::time::{Duration, Instant};

    let started = Instant::now();
    // La référence est relevée après 30 s, pas au démarrage : les tampons
    // d'encodage et les pistes ne sont pas encore alloués à l'instant zéro, et
    // les mesurer trop tôt fait passer une montée en régime normale pour une
    // fuite.
    let mut baseline = 0u64;
    let mut peak = 0u64;
    let mut saves = 0u32;
    let mut save_ms_total = 0.0;
    // Tous les relevés, pour comparer les creux plutôt que les instantanés.
    let mut samples: Vec<u64> = Vec::new();
    tracing::info!("campagne d'endurance : {seconds:.0}s (référence mémoire à t=30s)");

    let mut next_report = Instant::now() + Duration::from_secs(30);
    let mut next_save = Instant::now() + Duration::from_secs(120);

    while started.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(500));
        let rss = rss_mo();
        if baseline == 0 && started.elapsed().as_secs() >= 30 {
            baseline = rss;
            peak = rss;
            tracing::info!("référence mémoire : {baseline} Mo");
        }
        peak = peak.max(rss);
        if baseline > 0 {
            samples.push(rss);
        }

        // Une sauvegarde toutes les deux minutes : c'est le chemin le plus
        // coûteux du moteur, et celui qu'une fuite ferait dériver en premier.
        if Instant::now() >= next_save {
            next_save = Instant::now() + Duration::from_secs(120);
            match recorder.save(output.join(clip_name())) {
                Ok(outcome) => {
                    saves += 1;
                    save_ms_total += outcome.total_ms();
                    tracing::info!(
                        "sauvegarde #{saves} : {:.1}s en {:.0} ms",
                        outcome.seconds,
                        outcome.total_ms()
                    );
                }
                Err(e) => tracing::error!("sauvegarde en échec : {e:#}"),
            }
        }

        if Instant::now() >= next_report {
            next_report = Instant::now() + Duration::from_secs(30);
            let health = recorder.health();
            tracing::info!(
                "t={:.0}s  RSS={rss} Mo  erreurs={}  sautées={}  redémarrages={}  actif={}",
                started.elapsed().as_secs_f64(),
                health.write_errors,
                health.skipped_frames,
                health.restarts,
                health.running
            );
            if health.stalled() {
                anyhow::bail!(
                    "le buffer est figé depuis {:.0}s",
                    health.stalled_ms as f64 / 1000.0
                );
            }
            if !health.running {
                anyhow::bail!(
                    "le buffer s'est arrêté : {}",
                    health.failure.unwrap_or_else(|| "raison inconnue".into())
                );
            }
        }
    }

    let health = recorder.health();

    // La croissance se juge sur les **creux**, pas sur le dernier relevé.
    //
    // Les sauvegardes provoquent des pics transitoires de plusieurs dizaines de
    // mégaoctets ; terminer la campagne juste après l'un d'eux faisait conclure
    // à une fuite alors que la mémoire redescendait ensuite. Le minimum de
    // chaque moitié reflète l'état stable, seul révélateur d'une accumulation.
    let middle = samples.len() / 2;
    let low = |slice: &[u64]| slice.iter().copied().min().unwrap_or(0);
    let (early, late) = if middle > 0 {
        (low(&samples[..middle]), low(&samples[middle..]))
    } else {
        (baseline, rss_mo())
    };
    let growth = late as i64 - early as i64;

    tracing::info!("─── campagne terminée ───");
    tracing::info!("durée            : {:.0}s", started.elapsed().as_secs_f64());
    tracing::info!("RSS t=30s → fin  : {baseline} → {} Mo (pic {peak})", rss_mo());
    tracing::info!("creux 1re moitié → 2e : {early} → {late} Mo");
    tracing::info!("croissance       : {growth:+} Mo");
    tracing::info!("erreurs écriture : {}", health.write_errors);
    tracing::info!("images sautées   : {} (régulation)", health.skipped_frames);
    tracing::info!("redémarrages     : {}", health.restarts);
    if saves > 0 {
        tracing::info!(
            "sauvegardes      : {saves}, moyenne {:.0} ms",
            save_ms_total / saves as f64
        );
    }
    // 50 Mo de dérive sur une campagne : au-delà, quelque chose s'accumule.
    if growth > 50 {
        tracing::error!("❌ croissance mémoire suspecte");
    } else {
        tracing::info!("✅ mémoire stable");
    }
    Ok(())
}

fn clip_name() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("clip_{stamp}.mp4")
}

fn report(outcome: &smartclip_engine::SaveOutcome) {
    tracing::info!(
        "✅ {} — {:.1}s, {:.0} Mo, {} pistes ({}), sauvegardé en {:.0} ms \
         (finalisation {:.0} ms + recollage {:.0} ms)",
        outcome.path.display(),
        outcome.seconds,
        outcome.bytes as f64 / 1_048_576.0,
        outcome.tracks.len(),
        outcome.tracks.join(", "),
        outcome.total_ms(),
        outcome.flush_ms,
        outcome.concat_ms
    );
}

fn dirs_videos() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Videos"))
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Sous-commande `mix` : rééquilibre les pistes d'un clip et exporte.
///
/// C'est ce que le mixeur de l'interface appellera, un fader par gain.
fn run_mix(mut args: impl Iterator<Item = String>) -> Result<()> {
    let input = PathBuf::from(args.next().context("usage : smartclip mix <clip.mp4> [options]")?);
    let mut output = input.with_file_name(format!(
        "{}_mix.mp4",
        input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "clip".into())
    ));
    let mut gains: Vec<f32> = Vec::new();

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--output" | "-o" => {
                output = PathBuf::from(args.next().context("valeur manquante après --output")?)
            }
            "--gains" => {
                // « 1.0,0.5,0 » — un gain par piste, dans l'ordre du fichier.
                gains = args
                    .next()
                    .context("valeur manquante après --gains")?
                    .split(',')
                    .map(|g| g.trim().parse::<f32>())
                    .collect::<Result<_, _>>()?;
            }
            other => bail!("option inconnue : {other}"),
        }
    }

    // Le mixage tourne sur le thread principal, hors du moteur : c'est donc ici
    // qu'il faut initialiser COM et Media Foundation.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let result = run_mix_inner(&input, &output, gains);
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn run_mix_inner(input: &std::path::Path, output: &std::path::Path, mut gains: Vec<f32>) -> Result<()> {
    let info = smartclip_engine::export::inspect(input)?;
    tracing::info!(
        "{} — {:.1}s, {} piste(s) audio",
        input.display(),
        info.duration_hns as f64 / 10_000_000.0,
        info.audio_streams.len()
    );

    // Sans réglage explicite, toutes les pistes passent à l'identique : le
    // fichier reste fidèle à l'enregistrement.
    if gains.is_empty() {
        gains = vec![1.0; info.audio_streams.len()];
    }

    let outcome = smartclip_engine::mix_and_export(input, output, &gains)?;
    tracing::info!(
        "✅ {} — {:.1}s, {:.0} Mo, {} piste(s) mixée(s), crête {:.2}",
        output.display(),
        outcome.seconds,
        outcome.bytes as f64 / 1_048_576.0,
        outcome.tracks_mixed,
        outcome.peak
    );
    if outcome.clipped() {
        tracing::warn!(
            "le mixage dépassait la pleine échelle (crête {:.2}) : un limiteur a été appliqué. \
             Baisse les pistes les plus fortes pour un rendu plus propre.",
            outcome.peak
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut raw = std::env::args().skip(1).peekable();
    match raw.peek().map(String::as_str) {
        Some("mix") => {
            raw.next();
            return run_mix(raw);
        }
        // Extrait les pistes en WAV : c'est ce que l'éditeur charge pour
        // l'écoute en direct, et de quoi le vérifier sans interface.
        Some("tracks") => {
            raw.next();
            let input = PathBuf::from(raw.next().context("usage : smartclip tracks <clip.mp4>")?);
            let out = raw
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("smartclip_tracks"));
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            }
            let result = smartclip_engine::export::extract_tracks(&input, &out);
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
            for path in result? {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                println!("{}  {:.1} Mo", path.display(), size as f64 / 1_048_576.0);
            }
            return Ok(());
        }
        // Analyse la régularité des horodatages vidéo d'un clip.
        //
        // Un lecteur qui bloque sur une image tout en poursuivant le son trahit
        // presque toujours une timeline incohérente : c'est ce que cette
        // commande permet de constater plutôt que de supposer.
        Some("probe") => {
            raw.next();
            let input = PathBuf::from(raw.next().context("usage : smartclip probe <clip.mp4>")?);
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            }
            let result = smartclip_engine::export::probe_video(&input);
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
            let gaps = result?;
            if gaps.is_empty() {
                println!("aucune image lue");
                return Ok(());
            }
            let count = gaps.len();
            let mean: f64 = gaps.iter().sum::<f64>() / count as f64;
            let max = gaps.iter().cloned().fold(f64::MIN, f64::max);
            let irreguliers = gaps.iter().filter(|g| **g > mean * 1.5).count();
            println!("{count} intervalles entre images");
            println!("moyen     : {mean:.1} ms  (soit {:.1} images/s)", 1000.0 / mean);
            println!("maximum   : {max:.1} ms");
            println!(
                "irréguliers : {irreguliers} ({:.2} %) au-delà de 1,5× la moyenne",
                irreguliers as f64 / count as f64 * 100.0
            );
            let pires: Vec<String> = {
                let mut tries = gaps.clone();
                tries.sort_by(|a, b| b.partial_cmp(a).unwrap());
                tries.iter().take(5).map(|g| format!("{g:.0} ms")).collect()
            };
            println!("pires écarts : {}", pires.join(", "));
            return Ok(());
        }
        Some("list") => {
            raw.next();
            let dir = raw
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| dirs_videos().join("SmartClip"));
            return run_list(&dir);
        }
        _ => {}
    }

    let args = Args::parse()?;
    recorder::validate(&args.config)?;
    std::fs::create_dir_all(&args.output)?;

    tracing::info!(
        "démarrage du buffer : {:.0}s max, plafond {} Mo, segments de {:.0}s",
        args.config.buffer_seconds,
        args.config.max_bytes / 1_048_576,
        args.config.segment_seconds
    );

    let recorder = Recorder::start(args.config)?;
    tracing::info!("pistes audio détectées : {}", recorder.tracks().join(", "));

    if let Some(seconds) = args.duration {
        return endurance(&recorder, seconds, &args.output);
    }

    if let Some(delay) = args.auto_save {
        tracing::info!("mode vérification : sauvegarde automatique dans {delay:.0}s");
        std::thread::sleep(std::time::Duration::from_secs_f64(delay));
        let outcome = recorder.save(args.output.join(clip_name()))?;
        report(&outcome);
        return Ok(());
    }

    tracing::info!("prêt — Ctrl+Shift+X pour sauvegarder, Ctrl+C pour quitter");

    // Le raccourci est enregistré sur ce thread, qui doit donc porter la boucle
    // de messages : Windows délivre WM_HOTKEY à la file du thread appelant.
    // MOD_NOREPEAT évite qu'une pression maintenue déclenche une rafale de
    // sauvegardes.
    unsafe {
        RegisterHotKey(None, HOTKEY_ID, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, b'X' as u32)
            .context("Ctrl+Shift+X est déjà pris par une autre application")?;
    }

    let mut message = MSG::default();
    // GetMessageW renvoie 0 sur WM_QUIT et -1 en cas d'erreur.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.message != WM_HOTKEY || message.wParam.0 as i32 != HOTKEY_ID {
            continue;
        }
        match recorder.save(args.output.join(clip_name())) {
            Ok(outcome) => report(&outcome),
            Err(e) => tracing::error!("sauvegarde impossible : {e:#}"),
        }
    }

    unsafe {
        let _ = UnregisterHotKey(None, HOTKEY_ID);
    }
    Ok(())
}
