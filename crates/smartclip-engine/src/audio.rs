//! Découverte et capture des sources audio, une piste par application.
//!
//! C'est le différenciateur du produit : là où OBS impose d'ajouter chaque
//! source à la main, on énumère les sessions du périphérique de sortie et on
//! ouvre un client de loopback par processus, sans aucune configuration.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};

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
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, PCWSTR, PWSTR, Ref, implement};

use smartclip_core::clock::{MasterClock, QpcInstant};

use crate::{CHANNELS, SAMPLE_RATE, hns_since_boot};

/// `WAVE_FORMAT_IEEE_FLOAT` : non exposé par windows-rs dans un module
/// accessible ici, et figé par la spec RIFF.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// Un paquet audio daté sur la timeline commune.
pub struct AudioChunk {
    pub track: usize,
    /// PCM entrelacé 16 bits : le seul format accepté en entrée par l'encodeur
    /// AAC de Media Foundation, qui refuse le flottant.
    pub pcm: Vec<i16>,
    /// Position en 100 ns depuis le démarrage machine.
    pub boot_hns: i64,
}

/// Une source audio identifiée, telle qu'elle apparaîtra dans l'éditeur.
#[derive(Debug, Clone)]
pub struct Source {
    /// Libellé affiché. Unique dans une session : quatre pistes nommées « Jeu »
    /// seraient inutilisables dans le mixeur.
    pub label: String,
    /// Exécutable d'origine, conservé pour désambiguïser et pour le diagnostic.
    pub process: String,
    /// `None` pour le micro, qui se capture par le chemin WASAPI classique.
    pub pid: Option<u32>,
}

/// Utilitaires qui ouvrent une session audio sans jamais produire de son utile :
/// pilotes de périphériques, suites constructeur, surcouches de capture.
///
/// Sans ce filtre ils monopolisent les emplacements de capture — un essai réel
/// a vu trois d'entre eux occuper trois places sur quatre, ne laissant celle du
/// jeu qu'au hasard de l'ordre d'énumération. Une piste vide n'est pas neutre :
/// elle coûte un flux AAC et encombre le mixeur.
///
/// La liste ne peut pas être exhaustive ; elle n'a pas à l'être. Une inconnue
/// passe le filtre et se retrouve simplement dans le mixeur, ce qui est le
/// comportement souhaitable quand on hésite.
const UTILITIES: &[&str] = &[
    "amdnoisesuppression.exe",
    "amdrsserv.exe",
    "amdow.exe",
    "nvcontainer.exe",
    "nvidia share.exe",
    "nvidia web helper.exe",
    "roccat_swarm_monitor.exe",
    "icue.exe",
    "logioptionsplus_agent.exe",
    "steelseriesengine.exe",
    "razer synapse service.exe",
    "audiodg.exe",
    "voicemeeter.exe",
    "steamwebhelper.exe",
];

pub fn is_utility(process: &str) -> bool {
    UTILITIES.contains(&process.to_ascii_lowercase().as_str())
}

/// Classification automatique par exécutable.
///
/// Les inconnues tombent dans « Jeu » plutôt que dans un fourre-tout : dans le
/// cas d'usage visé, l'application bruyante non identifiée *est* le jeu. Rien
/// n'est jamais perdu, tout son capté aboutit dans une piste.
pub fn classify(process: &str) -> &'static str {
    match process.to_ascii_lowercase().as_str() {
        "discord.exe" | "vencord.exe" | "ts3client_win64.exe" | "teamspeak.exe" => "Discord",
        "spotify.exe" | "applemusic.exe" | "itunes.exe" | "foobar2000.exe" => "Musique",
        "chrome.exe" | "firefox.exe" | "msedge.exe" | "brave.exe" => "Navigateur",
        _ => "Jeu",
    }
}

/// Énumère les processus qui rendent du son sur la sortie par défaut.
pub fn discover(max: usize) -> Result<Vec<Source>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;

        let self_pid = GetCurrentProcessId();
        let mut found: Vec<Source> = Vec::new();
        for i in 0..sessions.GetCount()? {
            if found.len() >= max {
                break;
            }
            let Ok(control) = sessions.GetSession(i)?.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let pid = control.GetProcessId()?;
            // Nous exclure nous-mêmes n'est pas une coquetterie : ouvrir un
            // client de capture crée une session sur le périphérique, que le
            // balayage suivant détecte comme une application à enregistrer. Le
            // moteur s'attribuait ainsi une piste à lui-même.
            if pid == 0 || pid == self_pid {
                continue;
            }
            let process = process_name(pid).unwrap_or_else(|| format!("pid {pid}"));
            if is_utility(&process) {
                tracing::debug!("source ignorée (utilitaire) : {process}");
                continue;
            }
            // Dédoublonnage par **exécutable**, pas par PID.
            //
            // Discord, Steam ou un navigateur ouvrent plusieurs processus de
            // même nom qui rendent chacun du son. Comme le loopback est activé
            // en mode « arbre de processus », le premier client capte déjà tout
            // l'ensemble : un second emplacement ne rapporterait rien et
            // produirait deux faders au libellé identique dans le mixeur.
            if found
                .iter()
                .any(|s| s.process.eq_ignore_ascii_case(&process))
            {
                tracing::debug!("session ignorée : {process} a déjà un emplacement");
                continue;
            }
            found.push(Source {
                label: classify(&process).to_string(),
                process,
                pid: Some(pid),
            });
        }
        disambiguate(&mut found);
        Ok(found)
    }
}

/// Rend les libellés uniques.
///
/// Plusieurs applications tombent souvent dans la même catégorie — c'est
/// systématique pour « Jeu », qui sert de filet aux exécutables inconnus. Quand
/// une catégorie se répète, on lui accole le nom de l'exécutable ; quand elle
/// est seule, on garde le libellé court, plus lisible dans le mixeur.
pub(crate) fn disambiguate(sources: &mut [Source]) {
    let counts: Vec<usize> = sources
        .iter()
        .map(|s| sources.iter().filter(|o| o.label == s.label).count())
        .collect();
    for (source, count) in sources.iter_mut().zip(counts) {
        if count > 1 {
            let short = source.process.trim_end_matches(".exe");
            source.label = format!("{} ({short})", source.label);
        }
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
        let _ = CloseHandle(handle);
        ok.then(|| {
            let full = String::from_utf16_lossy(&buffer[..len as usize]);
            Path::new(&full)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .flatten()
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: HANDLE,
}
unsafe impl Send for ActivationHandler {}
unsafe impl Sync for ActivationHandler {}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe { windows::Win32::System::Threading::SetEvent(self.done) }
    }
}

fn activate_process_loopback(pid: u32) -> Result<IAudioClient> {
    unsafe {
        // Le blob doit vivre sur le tas et n'est jamais libéré.
        //
        // Le service audio relit la structure APRÈS avoir signalé la fin de
        // l'activation. Sur la pile, le cadre est déjà recyclé et le service
        // écrit dans de la mémoire réutilisée : le symptôme est un
        // STATUS_HEAP_CORRUPTION (0xC0000374) qui ne tombe qu'à la sortie du
        // processus, alors que la capture, elle, semble fonctionner.
        // Douze octets par piste et par session : le coût est nul.
        let params = Box::leak(Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    // Indispensable : un navigateur ou un launcher rend son
                    // audio depuis un processus enfant.
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        }));

        let mut variant = PROPVARIANT::default();
        {
            let inner = &mut variant.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob.cbSize =
                std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
            inner.Anonymous.blob.pBlobData = (params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast();
        }

        let done = CreateEventW(None, false, false, PCWSTR::null())?;
        let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler { done }.into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &handler,
        )?;

        let waited = WaitForSingleObject(done, 5_000);
        let _ = CloseHandle(done);
        if waited != WAIT_OBJECT_0 {
            bail!("délai dépassé à l'activation du loopback pour le pid {pid}");
        }

        let mut hr = windows::core::HRESULT(0);
        let mut unknown = None;
        operation.GetActivateResult(&mut hr, &mut unknown)?;
        hr.ok()
            .with_context(|| format!("loopback refusé pour le pid {pid}"))?;
        Ok(unknown.context("activation sans interface")?.cast()?)
    }
}

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

/// Capture une source jusqu'à ce que `stop` passe à vrai, en envoyant chaque
/// paquet au muxeur.
///
/// Une application silencieuse produit tout de même un flux continu de zéros à
/// cadence pleine : toutes les pistes ont donc rigoureusement la même longueur
/// et le muxeur n'a aucun trou à combler.
pub fn capture_source(
    track: usize,
    pid: Option<u32>,
    clock: MasterClock,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    tx: SyncSender<AudioChunk>,
) {
    // `alive` retombe à faux quoi qu'il arrive — sortie normale, erreur ou
    // panique. C'est ce qui permet au moteur de rendre l'emplacement au silence
    // : un flux déclaré que plus personne n'alimente empêche la finalisation du
    // segment, et la panne d'un micro suffirait à figer tout l'enregistrement.
    struct AliveGuard(Arc<AtomicBool>);
    impl Drop for AliveGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    let _guard = AliveGuard(alive);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut dropped = 0u64;
    let result = (|| -> Result<()> {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 4,
            nBlockAlign: CHANNELS * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        let (client, loopback) = match pid {
            Some(pid) => (activate_process_loopback(pid)?, true),
            None => (activate_microphone()?, false),
        };
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }

        unsafe {
            // 200 ms de tampon : assez large pour absorber une préemption du
            // thread sans jamais perdre de paquet.
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, None)?;
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            client.SetEventHandle(event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;

            while !stop.load(Ordering::Relaxed) {
                // Garde de 200 ms : si le périphérique se tait complètement, on
                // reboucle pour retester `stop` au lieu de rester coincé.
                if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                    continue;
                }
                while capture.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut packet_flags, mut qpc) = (0u32, 0u32, 0u64);
                    capture.GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut packet_flags,
                        None,
                        Some(&mut qpc),
                    )?;

                    let silent = packet_flags & 0x2 != 0 || data.is_null();
                    let count = frames as usize * CHANNELS as usize;
                    let pcm: Vec<i16> = if silent {
                        vec![0; count]
                    } else {
                        std::slice::from_raw_parts(data as *const f32, count)
                            .iter()
                            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                            .collect()
                    };
                    capture.ReleaseBuffer(frames)?;

                    let boot_hns = hns_since_boot(QpcInstant::from_u64(qpc).0, clock.frequency());
                    // Envoi non bloquant sur un canal borné.
                    //
                    // Si le muxeur cesse de consommer — encodeur saturé, GPU
                    // pris par un jeu — un canal non borné enflerait sans
                    // limite : une campagne a vu 1,8 Go s'accumuler ainsi.
                    // Mieux vaut perdre des paquets audio, dont on saura qu'ils
                    // manquent, que la mémoire de la machine.
                    match tx.try_send(AudioChunk { track, pcm, boot_hns }) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            dropped += 1;
                            if dropped % 500 == 1 {
                                tracing::warn!(
                                    "piste {track} : {dropped} paquet(s) écarté(s), le muxeur ne suit pas"
                                );
                            }
                        }
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                }
            }
            client.Stop()?;
        }
        Ok(())
    })();

    if let Err(e) = result {
        tracing::error!("piste audio {track} interrompue : {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(process: &str) -> Source {
        Source {
            label: classify(process).to_string(),
            process: process.to_string(),
            pid: Some(1),
        }
    }

    #[test]
    fn les_applications_connues_sont_reconnues() {
        assert_eq!(classify("Discord.exe"), "Discord");
        assert_eq!(classify("spotify.exe"), "Musique");
        assert_eq!(classify("chrome.exe"), "Navigateur");
    }

    #[test]
    fn une_application_inconnue_devient_le_jeu() {
        // Le filet par defaut : dans le cas d'usage vise, l'application
        // bruyante non identifiee EST le jeu.
        assert_eq!(classify("RocketLeague.exe"), "Jeu");
        assert_eq!(classify("un_truc_jamais_vu.exe"), "Jeu");
    }

    #[test]
    fn les_utilitaires_constructeur_sont_ecartes() {
        assert!(is_utility("AMDNoiseSuppression.exe"));
        assert!(is_utility("ROCCAT_Swarm_Monitor.exe"));
        // La comparaison ignore la casse : Windows ne la normalise pas.
        assert!(is_utility("AmdRSServ.EXE"));
        assert!(!is_utility("RocketLeague.exe"));
    }

    #[test]
    fn les_libelles_en_double_recoivent_le_nom_de_l_executable() {
        let mut sources = vec![source("RocketLeague.exe"), source("Valorant.exe")];
        disambiguate(&mut sources);
        assert_eq!(sources[0].label, "Jeu (RocketLeague)");
        assert_eq!(sources[1].label, "Jeu (Valorant)");
    }

    #[test]
    fn un_libelle_unique_reste_court() {
        // Quatre faders nommes "Jeu" seraient inutilisables, mais alourdir un
        // libelle sans ambiguite nuirait a la lisibilite du mixeur.
        let mut sources = vec![source("RocketLeague.exe"), source("Discord.exe")];
        disambiguate(&mut sources);
        assert_eq!(sources[0].label, "Jeu");
        assert_eq!(sources[1].label, "Discord");
    }

    #[test]
    fn les_libelles_restent_uniques_apres_desambiguisation() {
        let mut sources = vec![
            source("chrome.exe"),
            source("firefox.exe"),
            source("Discord.exe"),
        ];
        disambiguate(&mut sources);
        let mut labels: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 3);
    }
}
