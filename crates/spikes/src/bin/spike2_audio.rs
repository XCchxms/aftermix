//! Spike 2 — loopback audio par processus, N sources séparées.
//!
//! Risque validé : R2 (le différenciateur produit). Question posée : peut-on
//! découvrir tout seul les applications qui émettent du son et capturer chacune
//! dans sa propre piste, sans que l'utilisateur configure quoi que ce soit ?
//!
//! Mécanisme : `ActivateAudioInterfaceAsync` sur le pseudo-périphérique
//! `VAD\Process_Loopback`, paramétré par un `AUDIOCLIENT_ACTIVATION_PARAMS`
//! passé en `VT_BLOB`. C'est ce que fait l'« Application Audio Capture » d'OBS.
//! Documenté par Microsoft à partir du build 20348 — on cible Windows 11.
//!
//! Le spike relève aussi le QPC du premier paquet de chaque piste : c'est la
//! donnée d'entrée du Spike 3 sur la synchronisation.
//!
//! Usage : `cargo run --release --bin spike2_audio -- --seconds 20`

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, PCWSTR, PWSTR, Ref, implement};

use aftermix_core::clock::{MasterClock, QpcInstant};

/// Format de travail commun à toutes les pistes.
///
/// Le loopback par processus ne supporte pas `GetMixFormat` : contrairement à
/// une capture classique, il faut imposer le format. On prend du flottant 32
/// bits, seul format qui n'introduit aucune perte au mixage ultérieur.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// `WAVE_FORMAT_IEEE_FLOAT` — la constante n'est pas exposée par windows-rs
/// dans un module accessible ici, et sa valeur est figée par la spec RIFF.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

fn work_format() -> WAVEFORMATEX {
    let bits = 32u16;
    let block_align = CHANNELS * bits / 8;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

// ─────────────────────────── découverte des sources ───────────────────────────

#[derive(Debug, Clone)]
struct DiscoveredSource {
    pid: u32,
    /// Nom d'exécutable, sans le chemin.
    process: String,
    /// Le bucket auquel la classification automatique l'a affecté.
    bucket: &'static str,
}

/// Classification automatique par exécutable.
///
/// C'est le cœur de l'argument « aucune configuration » face à OBS, où chaque
/// source doit être ajoutée à la main. Les inconnues tombent dans « Autres »,
/// ce qui garantit qu'aucun son n'est jamais perdu.
fn classify(process: &str) -> &'static str {
    let p = process.to_ascii_lowercase();
    match p.as_str() {
        "discord.exe" | "vencord.exe" | "ts3client_win64.exe" | "teamspeak.exe" => "Discord",
        "spotify.exe" | "applemusic.exe" | "itunes.exe" | "foobar2000.exe" => "Musique",
        "chrome.exe" | "firefox.exe" | "msedge.exe" | "brave.exe" => "Navigateur",
        _ => "Autres",
    }
}

/// Énumère les processus qui rendent du son sur le périphérique de sortie par
/// défaut, via `IAudioSessionManager2`.
fn discover_sources() -> Result<Vec<DiscoveredSource>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;

        let mut found: Vec<DiscoveredSource> = Vec::new();
        for i in 0..sessions.GetCount()? {
            let control: IAudioSessionControl2 = match sessions.GetSession(i)?.cast() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let pid = control.GetProcessId()?;
            if pid == 0 {
                continue; // session système
            }
            // Une même application ouvre souvent plusieurs sessions ; le
            // loopback capture déjà tout l'arbre de processus, un client par
            // PID suffit.
            if found.iter().any(|s| s.pid == pid) {
                continue;
            }
            let process = process_name(pid).unwrap_or_else(|| format!("pid {pid}"));
            let bucket = classify(&process);
            found.push(DiscoveredSource {
                pid,
                process,
                bucket,
            });
        }
        Ok(found)
    }
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 260];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if !ok {
            return None;
        }
        let full = String::from_utf16_lossy(&buffer[..len as usize]);
        Some(
            Path::new(&full)
                .file_name()?
                .to_string_lossy()
                .into_owned(),
        )
    }
}

// ──────────────────────── activation du loopback par PID ──────────────────────

/// `ActivateAudioInterfaceAsync` rend la main immédiatement et rappelle ce
/// handler depuis un thread MTA du pool audio. On se contente d'y signaler un
/// événement : toute la logique reste sur le thread appelant.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: HANDLE,
}

// Le handler ne porte qu'un HANDLE d'événement, dont la signalisation est
// thread-safe par construction.
unsafe impl Send for ActivationHandler {}
unsafe impl Sync for ActivationHandler {}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe { windows::Win32::System::Threading::SetEvent(self.done) }
    }
}

/// Ouvre un `IAudioClient` qui ne capte que l'audio rendu par `pid` et ses
/// enfants.
fn activate_process_loopback(pid: u32) -> Result<IAudioClient> {
    unsafe {
        // Le blob est alloué sur le tas et volontairement fuité.
        //
        // L'activation est asynchrone : rien dans le contrat de l'API ne
        // garantit que le service audio a fini de lire la structure quand
        // `ActivateCompleted` signale l'événement. Avec un `params` sur la pile,
        // le cadre est réutilisé dès le retour de la fonction et le service
        // écrit dans de la mémoire recyclée — ce qui se manifestait en
        // STATUS_HEAP_CORRUPTION (0xC0000374). Douze octets par piste,
        // une fois pour toute la session : le prix est nul.
        let params = Box::leak(Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    // INCLUDE_TARGET_PROCESS_TREE est indispensable : un
                    // navigateur ou un launcher rend son audio depuis un
                    // processus enfant.
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        }));

        // Le paramètre se transmet en VT_BLOB : c'est la seule façon prévue par
        // l'API de passer une structure à l'activation.
        let mut variant = PROPVARIANT::default();
        {
            let inner = &mut variant.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob.cbSize =
                std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
            inner.Anonymous.blob.pBlobData = (params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast();
        }

        let done = CreateEventW(None, false, false, PCWSTR::null())?;
        let handler: IActivateAudioInterfaceCompletionHandler =
            ActivationHandler { done }.into();

        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &handler,
        )
        .context("ActivateAudioInterfaceAsync")?;

        let waited = WaitForSingleObject(done, 5_000);
        let _ = CloseHandle(done);
        if waited != WAIT_OBJECT_0 {
            bail!("délai dépassé à l'activation du loopback pour le pid {pid}");
        }

        let mut hr = windows::core::HRESULT(0);
        let mut unknown = None;
        operation.GetActivateResult(&mut hr, &mut unknown)?;
        hr.ok().with_context(|| {
            format!("le loopback a été refusé pour le pid {pid} (HRESULT {:#010x})", hr.0)
        })?;

        unknown
            .context("activation sans interface")?
            .cast::<IAudioClient>()
            .context("cast IAudioClient")
    }
}

/// Le micro se capture par le chemin WASAPI classique — pas de loopback ici.
fn activate_microphone() -> Result<IAudioClient> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .context("aucun périphérique d'entrée par défaut")?;
        Ok(device.Activate(CLSCTX_ALL, None)?)
    }
}

// ──────────────────────────────── capture ─────────────────────────────────────

#[derive(Debug, Default)]
struct TrackStats {
    frames: u64,
    /// Nombre de paquets marqués silencieux par WASAPI.
    silent_packets: u64,
    /// Ruptures de continuité signalées par le pilote : chacune est un trou
    /// dans la timeline qu'il faudra combler au muxage (Spike 3).
    discontinuities: u64,
    peak: f32,
    first_qpc: Option<QpcInstant>,
    last_qpc: Option<QpcInstant>,
}

/// Boucle de capture commune au loopback et au micro.
fn capture_loop(
    client: &IAudioClient,
    format: &WAVEFORMATEX,
    loopback: bool,
    stop: &AtomicBool,
    wav: &mut WavWriter,
) -> Result<TrackStats> {
    let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    if loopback {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }

    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                // 200 ms : assez large pour absorber une préemption du thread
                // sans jamais perdre de paquet.
                2_000_000,
                0,
                format,
                None,
            )
            .context("IAudioClient::Initialize")?;

        let event = CreateEventW(None, false, false, PCWSTR::null())?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;

        let mut stats = TrackStats::default();
        let block_align = format.nBlockAlign as usize;

        while !stop.load(Ordering::Relaxed) {
            // 200 ms de garde : si le périphérique se tait complètement, on
            // reboucle pour retester `stop` au lieu de rester coincé.
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }

            loop {
                let available = capture.GetNextPacketSize()?;
                if available == 0 {
                    break;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut packet_flags = 0u32;
                let mut qpc = 0u64;
                capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut packet_flags,
                    None,
                    Some(&mut qpc),
                )?;

                let silent = packet_flags & 0x2 != 0; // AUDCLNT_BUFFERFLAGS_SILENT
                let discontinuity = packet_flags & 0x1 != 0;
                if discontinuity {
                    stats.discontinuities += 1;
                }

                let bytes = frames as usize * block_align;
                let samples: &[f32] = if silent || data.is_null() {
                    stats.silent_packets += 1;
                    &[]
                } else {
                    std::slice::from_raw_parts(data as *const f32, bytes / 4)
                };

                for &s in samples {
                    let a = s.abs();
                    if a > stats.peak {
                        stats.peak = a;
                    }
                }

                if samples.is_empty() {
                    wav.write_silence(bytes)?;
                } else {
                    wav.write_samples(samples)?;
                }

                let instant = QpcInstant::from_u64(qpc);
                stats.first_qpc.get_or_insert(instant);
                stats.last_qpc = Some(instant);
                stats.frames += frames as u64;

                capture.ReleaseBuffer(frames)?;
            }
        }

        client.Stop()?;
        Ok(stats)
    }
}

// ──────────────────────────────── écriture WAV ────────────────────────────────

/// Écrivain RIFF/WAVE flottant 32 bits.
///
/// L'en-tête est réécrit à la fermeture, une fois la taille connue.
struct WavWriter {
    file: std::io::BufWriter<std::fs::File>,
    data_bytes: u32,
}

impl WavWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = std::fs::File::create(path)
            .with_context(|| format!("création de {}", path.display()))?;
        let mut writer = Self {
            file: std::io::BufWriter::new(file),
            data_bytes: 0,
        };
        writer.write_header(0)?;
        Ok(writer)
    }

    fn write_header(&mut self, data_bytes: u32) -> Result<()> {
        let block_align = CHANNELS * 4;
        let byte_rate = SAMPLE_RATE * block_align as u32;
        let f = &mut self.file;
        f.write_all(b"RIFF")?;
        // Charge utile RIFF : "WAVE" (4) + fmt de 18 octets (8+18) + fact (8+4)
        // + en-tête data (8) = 50, puis les échantillons.
        f.write_all(&(50u32 + data_bytes).to_le_bytes())?;
        f.write_all(b"WAVE")?;
        // fmt de 18 octets : exigé par la spec pour un format flottant.
        f.write_all(b"fmt ")?;
        f.write_all(&18u32.to_le_bytes())?;
        f.write_all(&WAVE_FORMAT_IEEE_FLOAT.to_le_bytes())?;
        f.write_all(&CHANNELS.to_le_bytes())?;
        f.write_all(&SAMPLE_RATE.to_le_bytes())?;
        f.write_all(&byte_rate.to_le_bytes())?;
        f.write_all(&block_align.to_le_bytes())?;
        f.write_all(&32u16.to_le_bytes())?;
        f.write_all(&0u16.to_le_bytes())?; // cbSize
        // fact : obligatoire pour les formats non-PCM.
        f.write_all(b"fact")?;
        f.write_all(&4u32.to_le_bytes())?;
        f.write_all(&(data_bytes / block_align as u32).to_le_bytes())?;
        f.write_all(b"data")?;
        f.write_all(&data_bytes.to_le_bytes())?;
        Ok(())
    }

    fn write_samples(&mut self, samples: &[f32]) -> Result<()> {
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(samples.as_ptr() as *const u8, std::mem::size_of_val(samples))
        };
        self.file.write_all(bytes)?;
        self.data_bytes += bytes.len() as u32;
        Ok(())
    }

    /// WASAPI signale les passages silencieux sans fournir de données : il faut
    /// tout de même écrire les zéros, sinon la piste raccourcit et se désynchronise.
    fn write_silence(&mut self, bytes: usize) -> Result<()> {
        const ZEROS: [u8; 4096] = [0u8; 4096];
        let mut left = bytes;
        while left > 0 {
            let n = left.min(ZEROS.len());
            self.file.write_all(&ZEROS[..n])?;
            left -= n;
        }
        self.data_bytes += bytes as u32;
        Ok(())
    }

    fn finish(mut self) -> Result<u32> {
        use std::io::{Seek, SeekFrom};
        self.file.flush()?;
        let data_bytes = self.data_bytes;
        let mut inner = self.file.into_inner()?;
        inner.seek(SeekFrom::Start(0))?;
        let mut rewrite = WavWriter {
            file: std::io::BufWriter::new(inner),
            data_bytes: 0,
        };
        rewrite.write_header(data_bytes)?;
        rewrite.file.flush()?;
        Ok(data_bytes)
    }
}

// ──────────────────────────────────── main ────────────────────────────────────

struct Args {
    seconds: u64,
    outdir: PathBuf,
    /// Bisection de la corruption de tas observée au premier essai : permet
    /// d'isoler le chemin loopback du chemin micro.
    with_loopback: bool,
    with_mic: bool,
    /// Ne capture rien : vérifie si la seule énumération des sessions suffit à
    /// corrompre le tas.
    discover_only: bool,
    /// Active les clients loopback puis les relâche aussitôt, sans `Initialize`
    /// ni capture : sépare l'activation de la boucle de lecture.
    activate_only: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Args {
            seconds: 20,
            outdir: std::env::temp_dir().join("aftermix_spike2"),
            with_loopback: true,
            with_mic: true,
            discover_only: false,
            activate_only: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || {
                it.next()
                    .with_context(|| format!("valeur manquante après {flag}"))
            };
            match flag.as_str() {
                "--seconds" => args.seconds = value()?.parse()?,
                "--outdir" => args.outdir = PathBuf::from(value()?),
                "--no-loopback" => args.with_loopback = false,
                "--no-mic" => args.with_mic = false,
                "--discover-only" => args.discover_only = true,
                "--activate-only" => args.activate_only = true,
                other => bail!("option inconnue : {other}"),
            }
        }
        Ok(args)
    }
}

struct TrackResult {
    label: String,
    stats: Result<TrackStats>,
    path: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse()?;
    std::fs::create_dir_all(&args.outdir)?;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let sources = discover_sources().context("énumération des sessions audio")?;
    if sources.is_empty() {
        tracing::warn!(
            "aucune application n'émet de son : lance un jeu, Spotify ou une vidéo, puis relance"
        );
    }
    tracing::info!("{} source(s) découverte(s) :", sources.len());
    for s in &sources {
        tracing::info!("  [{}] {} (pid {})", s.bucket, s.process, s.pid);
    }

    let clock = MasterClock::new();
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();

    if args.discover_only {
        tracing::info!("--discover-only : aucune capture, on s'arrête ici");
        unsafe { CoUninitialize() };
        return Ok(());
    }

    if args.activate_only {
        for source in &sources {
            match activate_process_loopback(source.pid) {
                Ok(client) => {
                    tracing::info!("activation OK pour {} (pid {})", source.process, source.pid);
                    drop(client);
                }
                Err(e) => tracing::error!("activation KO pour {} : {e:#}", source.process),
            }
        }
        tracing::info!("--activate-only : terminé sans capture");
        unsafe { CoUninitialize() };
        return Ok(());
    }

    // Une piste par source découverte, plus le micro.
    if args.with_loopback {
        for source in &sources {
            let stop = Arc::clone(&stop);
            let label = format!("{}-{}", source.bucket, source.process.trim_end_matches(".exe"));
            let path = args.outdir.join(format!("{label}.wav"));
            let pid = source.pid;
            handles.push(std::thread::spawn(move || {
                run_track(&label, &path, stop, Some(pid))
            }));
        }
    }
    if args.with_mic {
        let stop = Arc::clone(&stop);
        let path = args.outdir.join("Micro.wav");
        handles.push(std::thread::spawn(move || {
            run_track("Micro", &path, stop, None)
        }));
    }

    tracing::info!("capture de {} s en cours…", args.seconds);
    std::thread::sleep(std::time::Duration::from_secs(args.seconds));
    stop.store(true, Ordering::Relaxed);

    let results: Vec<TrackResult> = handles
        .into_iter()
        .filter_map(|h| h.join().ok())
        .collect();

    report(&results, &clock, args.seconds);

    unsafe { CoUninitialize() };
    Ok(())
}

fn run_track(
    label: &str,
    path: &Path,
    stop: Arc<AtomicBool>,
    pid: Option<u32>,
) -> TrackResult {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let result = (|| -> Result<TrackStats> {
        let format = work_format();
        let (client, loopback) = match pid {
            Some(pid) => (activate_process_loopback(pid)?, true),
            None => (activate_microphone()?, false),
        };
        let mut wav = WavWriter::create(path)?;
        let stats = capture_loop(&client, &format, loopback, &stop, &mut wav);
        wav.finish()?;
        stats
    })();

    TrackResult {
        label: label.to_string(),
        stats: result,
        path: path.to_path_buf(),
    }
}

fn report(results: &[TrackResult], clock: &MasterClock, seconds: u64) {
    tracing::info!("─── Spike 2 terminé ───");

    let mut ok = 0;
    let mut premiers: Vec<(String, i64)> = Vec::new();

    for r in results {
        match &r.stats {
            Ok(s) => {
                ok += 1;
                let duree = s.frames as f64 / SAMPLE_RATE as f64;
                let couverture = duree / seconds as f64 * 100.0;
                tracing::info!(
                    "{:<28} {:>7.2}s ({:>5.1}%)  crête {:>5.3}  silences {:<4} ruptures {}",
                    r.label,
                    duree,
                    couverture,
                    s.peak,
                    s.silent_packets,
                    s.discontinuities
                );
                if let Some(q) = s.first_qpc {
                    premiers.push((r.label.clone(), clock.hns_since_origin(q)));
                }
            }
            Err(e) => tracing::error!("{:<28} ÉCHEC : {e:#}", r.label),
        }
    }

    tracing::info!("{ok}/{} piste(s) capturée(s)", results.len());

    // Étalement des premiers horodatages : c'est l'écart de départ que le
    // muxeur du Spike 3 devra rattraper. S'il est important, il faut aligner
    // les pistes sur le QPC et non sur leur ordre d'arrivée.
    if premiers.len() > 1 {
        let min = premiers.iter().map(|(_, t)| *t).min().unwrap();
        let max = premiers.iter().map(|(_, t)| *t).max().unwrap();
        tracing::info!(
            "étalement des premiers paquets : {:.1} ms",
            (max - min) as f64 / 10_000.0
        );
        for (label, t) in &premiers {
            tracing::info!("  {label:<28} T+{:.1} ms", (*t - min) as f64 / 10_000.0);
        }
    }

    if let Some(dir) = results.first().and_then(|r| r.path.parent()) {
        tracing::info!("fichiers WAV : {}", dir.display());
    }
}
