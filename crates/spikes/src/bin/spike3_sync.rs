//! Spike 3 — muxage QPC de la vidéo et de N pistes audio. **RISQUE CRITIQUE R1.**
//!
//! Question posée : peut-on écrire un seul fichier contenant la vidéo et N pistes
//! audio indépendantes, toutes alignées sur la même horloge, sans dérive audible
//! au bout de 5 minutes ? Critère de sortie : **dérive < 40 ms à 5 min**.
//!
//! Deux hypothèses sont testées d'un coup :
//!
//! 1. **Une timeline unique.** `Direct3D11CaptureFrame::SystemRelativeTime`
//!    (vidéo) et `pu64QPCPosition` (audio) sont tous deux dérivés du QPC, donc
//!    convertibles en « 100 ns depuis le démarrage ». C'est cette conversion qui
//!    place toutes les sources sur un même référentiel — et non l'ordre
//!    d'arrivée des paquets, qui n'a aucune valeur temporelle.
//!
//! 2. **MP4 multi-pistes plutôt que MKV.** Si le SinkWriter accepte un flux
//!    vidéo et N flux AAC dans un même MP4, on économise à la fois une
//!    dépendance MKV et tout appel à ffmpeg sur le chemin du buffer.
//!
//! Architecture : un seul thread possède le `IMFSinkWriter` (il n'est pas
//! thread-safe). Les threads de capture audio lui envoient leurs paquets par
//! canal ; le thread muxeur capture la vidéo et draine les canaux. C'est aussi
//! la bonne architecture pour le moteur définitif.
//!
//! Usage : `cargo run --release --bin spike3_sync -- --minutes 5`

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
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
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSinkWriter,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SINK_WRITER_D3D_MANAGER, MF_SINK_WRITER_DISABLE_THROTTLING, MF_VERSION, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL,
    MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown, MFStartup,
    MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoInterlace_Progressive,
    eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HSTRING, Interface, PCWSTR, Ref, implement};

use aftermix_core::clock::{HNS_PER_SEC, MasterClock, QpcInstant};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const TEXTURE_RING: usize = 8;

/// Convertit des ticks QPC en « 100 ns depuis le démarrage de la machine ».
///
/// C'est le pivot de tout le spike : `SystemRelativeTime` de WGC est déjà dans
/// cette unité, et cette fonction y amène les horodatages audio. Les deux
/// familles de sources deviennent alors directement comparables.
fn hns_since_boot(ticks: i64, freq: i64) -> i64 {
    ((ticks as i128 * HNS_PER_SEC as i128) / freq as i128) as i64
}

// ────────────────────────────── messages du muxeur ────────────────────────────

struct AudioChunk {
    stream: u32,
    /// PCM entrelacé 16 bits — le format exigé par l'encodeur AAC de Media
    /// Foundation, qui refuse le flottant.
    pcm: Vec<i16>,
    /// Position sur la timeline commune, en 100 ns depuis le démarrage.
    boot_hns: i64,
}

/// Ce que chaque piste audio rapporte en fin de course.
#[derive(Debug, Default, Clone)]
struct AudioReport {
    label: String,
    frames: u64,
    first_boot_hns: i64,
    last_boot_hns: i64,
    /// Nombre de trames du dernier paquet, nécessaire pour clore la mesure sans
    /// biais — voir [`AudioReport::qpc_seconds`].
    last_packet_frames: u32,
    discontinuities: u64,
    peak: f32,
}

impl AudioReport {
    /// Durée déduite du nombre d'échantillons : ce que croit l'horloge du
    /// périphérique.
    fn sample_seconds(&self) -> f64 {
        self.frames as f64 / SAMPLE_RATE as f64
    }

    /// Durée déduite du QPC : le temps réellement écoulé.
    ///
    /// `last_boot_hns` date le *début* du dernier paquet ; sans y ajouter sa
    /// durée on sous-estime systématiquement d'une période WASAPI (10 ms par
    /// défaut). Le premier essai affichait +10,0 ms sur les quatre pistes, ce
    /// qui était ce biais et non une dérive.
    fn qpc_seconds(&self) -> f64 {
        let span = (self.last_boot_hns - self.first_boot_hns) as f64 / HNS_PER_SEC as f64;
        span + self.last_packet_frames as f64 / SAMPLE_RATE as f64
    }

    /// **La mesure qui décide du sort de l'architecture.**
    ///
    /// L'écart entre les deux. S'il croît avec la durée, une piste horodatée au
    /// compteur d'échantillons dérive par rapport à la vidéo — et c'est
    /// exactement le défaut que le produit est censé ne pas avoir.
    fn drift_ms(&self) -> f64 {
        (self.sample_seconds() - self.qpc_seconds()) * 1000.0
    }
}

// ─────────────────────────────── activation audio ─────────────────────────────

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: HANDLE,
}

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

fn activate_process_loopback(pid: u32) -> Result<IAudioClient> {
    unsafe {
        // Sur le tas et volontairement fuité : le service audio relit la
        // structure APRÈS le signalement de fin d'activation. Sur la pile, le
        // cadre est déjà recyclé → STATUS_HEAP_CORRUPTION. Constaté au Spike 2.
        let params = Box::leak(Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
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
            bail!("délai dépassé à l'activation du pid {pid}");
        }

        let mut hr = windows::core::HRESULT(0);
        let mut unknown = None;
        operation.GetActivateResult(&mut hr, &mut unknown)?;
        hr.ok()?;
        Ok(unknown.context("activation sans interface")?.cast()?)
    }
}

fn activate_microphone() -> Result<IAudioClient> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)?;
        Ok(device.Activate(CLSCTX_ALL, None)?)
    }
}

#[derive(Clone)]
struct SourceSpec {
    label: String,
    /// `None` = micro (capture classique), `Some(pid)` = loopback par processus.
    pid: Option<u32>,
    stream: u32,
}

fn discover_sources(max: usize) -> Result<Vec<(String, u32)>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;

        let mut found: Vec<(String, u32)> = Vec::new();
        for i in 0..sessions.GetCount()? {
            if found.len() >= max {
                break;
            }
            let Ok(control) = sessions.GetSession(i)?.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let pid = control.GetProcessId()?;
            if pid == 0 || found.iter().any(|(_, p)| *p == pid) {
                continue;
            }
            found.push((format!("pid{pid}"), pid));
        }
        Ok(found)
    }
}

// ──────────────────────────── thread de capture audio ─────────────────────────

fn audio_thread(
    spec: SourceSpec,
    clock: MasterClock,
    stop: Arc<AtomicBool>,
    tx: Sender<AudioChunk>,
) -> AudioReport {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut report = AudioReport {
        label: spec.label.clone(),
        ..Default::default()
    };

    let run = (|| -> Result<()> {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * (CHANNELS as u32) * 4,
            nBlockAlign: CHANNELS * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        let (client, loopback) = match spec.pid {
            Some(pid) => (activate_process_loopback(pid)?, true),
            None => (activate_microphone()?, false),
        };

        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }

        unsafe {
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, None)?;
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            client.SetEventHandle(event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;

            while !stop.load(Ordering::Relaxed) {
                if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                    continue;
                }
                loop {
                    if capture.GetNextPacketSize()? == 0 {
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

                    if packet_flags & 0x1 != 0 {
                        report.discontinuities += 1;
                    }
                    let silent = packet_flags & 0x2 != 0 || data.is_null();
                    let count = frames as usize * CHANNELS as usize;

                    // Conversion flottant → 16 bits : l'encodeur AAC de Media
                    // Foundation n'accepte pas le flottant en entrée.
                    let pcm: Vec<i16> = if silent {
                        vec![0i16; count]
                    } else {
                        let src = std::slice::from_raw_parts(data as *const f32, count);
                        src.iter()
                            .map(|&s| {
                                let a = s.abs();
                                if a > report.peak {
                                    report.peak = a;
                                }
                                (s.clamp(-1.0, 1.0) * 32767.0) as i16
                            })
                            .collect()
                    };
                    capture.ReleaseBuffer(frames)?;

                    let boot_hns =
                        hns_since_boot(QpcInstant::from_u64(qpc).0, clock.frequency());
                    if report.frames == 0 {
                        report.first_boot_hns = boot_hns;
                    }
                    report.last_boot_hns = boot_hns;
                    report.last_packet_frames = frames;
                    report.frames += frames as u64;

                    if tx
                        .send(AudioChunk {
                            stream: spec.stream,
                            pcm,
                            boot_hns,
                        })
                        .is_err()
                    {
                        return Ok(()); // le muxeur a fermé, on sort proprement
                    }
                }
            }
            client.Stop()?;
        }
        Ok(())
    })();

    if let Err(e) = run {
        tracing::error!("{} : {e:#}", spec.label);
    }
    report
}

// ───────────────────────────────── muxeur ─────────────────────────────────────

struct Muxer {
    writer: IMFSinkWriter,
    video_stream: u32,
}

fn create_muxer(
    device: &ID3D11Device,
    path: &str,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    audio_tracks: usize,
) -> Result<(Muxer, Vec<u32>)> {
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
    let manager = manager.context("IMFDXGIDeviceManager nul")?;
    unsafe { manager.ResetDevice(device, token)? };

    let mut attributes: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attributes, 3)? };
    let attributes = attributes.context("IMFAttributes nul")?;
    unsafe {
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
    }

    let writer =
        unsafe { MFCreateSinkWriterFromURL(&HSTRING::from(path), None, &attributes)? };

    // ── flux vidéo ──
    let out = unsafe { MFCreateMediaType()? };
    unsafe {
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
    }
    pack(&out, &MF_MT_FRAME_SIZE, width, height)?;
    pack(&out, &MF_MT_FRAME_RATE, fps, 1)?;
    pack(&out, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    let video_stream = unsafe { writer.AddStream(&out)? };

    let inp = unsafe { MFCreateMediaType()? };
    unsafe {
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    pack(&inp, &MF_MT_FRAME_SIZE, width, height)?;
    pack(&inp, &MF_MT_FRAME_RATE, fps, 1)?;
    pack(&inp, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    unsafe { writer.SetInputMediaType(video_stream, &inp, None)? };

    // ── flux audio : tous ajoutés avant BeginWriting ──
    let mut audio_streams = Vec::with_capacity(audio_tracks);
    for index in 0..audio_tracks {
        let out = unsafe { MFCreateMediaType()? };
        unsafe {
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?; // 192 kbps
        }
        let stream = unsafe { writer.AddStream(&out) }
            .with_context(|| format!("AddStream refusé pour la piste audio {index}"))?;

        let inp = unsafe { MFCreateMediaType()? };
        unsafe {
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (CHANNELS * 2) as u32)?;
            writer.SetInputMediaType(stream, &inp, None)?;
        }
        audio_streams.push(stream);
    }

    Ok((
        Muxer {
            writer,
            video_stream,
        },
        audio_streams,
    ))
}

fn pack(media_type: &IMFMediaType, key: &GUID, high: u32, low: u32) -> Result<()> {
    unsafe { media_type.SetUINT64(key, ((high as u64) << 32) | low as u64)? };
    Ok(())
}

fn write_video(
    muxer: &Muxer,
    texture: &ID3D11Texture2D,
    pts_hns: i64,
    duration_hns: i64,
) -> Result<()> {
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
        let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
        buffer.SetCurrentLength(length)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(pts_hns)?;
        sample.SetSampleDuration(duration_hns)?;
        muxer.writer.WriteSample(muxer.video_stream, &sample)?;
    }
    Ok(())
}

fn write_audio(muxer: &Muxer, chunk: &AudioChunk, pts_hns: i64) -> Result<()> {
    unsafe {
        let bytes = std::mem::size_of_val(&chunk.pcm[..]);
        let buffer = MFCreateMemoryBuffer(bytes as u32)?;
        let mut dst = std::ptr::null_mut();
        buffer.Lock(&mut dst, None, None)?;
        std::ptr::copy_nonoverlapping(chunk.pcm.as_ptr() as *const u8, dst, bytes);
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes as u32)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(pts_hns)?;
        let frames = chunk.pcm.len() / CHANNELS as usize;
        sample.SetSampleDuration(frames as i64 * HNS_PER_SEC / SAMPLE_RATE as i64)?;
        muxer.writer.WriteSample(chunk.stream, &sample)?;
    }
    Ok(())
}

// ─────────────────────────────────── main ─────────────────────────────────────

/// Origine des horodatages vidéo.
#[derive(Clone, Copy, PartialEq)]
enum VideoPts {
    /// Horloge QPC, la même que l'audio. **Le bon choix**, retenu par défaut.
    Qpc,
    /// Indice de frame × durée nominale. Conservé pour pouvoir remesurer la
    /// désynchro que ce mode provoque.
    Cfr,
}

struct Args {
    minutes: f64,
    fps: u32,
    bitrate: u32,
    out: String,
    max_sources: usize,
    video_pts: VideoPts,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Args {
            minutes: 1.0,
            fps: 60,
            bitrate: 20_000_000,
            out: std::env::temp_dir()
                .join("spike3_sync.mp4")
                .to_string_lossy()
                .into_owned(),
            max_sources: 4,
            video_pts: VideoPts::Qpc,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || {
                it.next()
                    .with_context(|| format!("valeur manquante après {flag}"))
            };
            match flag.as_str() {
                "--minutes" => args.minutes = value()?.parse()?,
                "--fps" => args.fps = value()?.parse()?,
                "--bitrate" => args.bitrate = value()?.parse()?,
                "--out" => args.out = value()?,
                "--max-sources" => args.max_sources = value()?.parse()?,
                "--video-pts" => {
                    args.video_pts = match value()?.as_str() {
                        "qpc" => VideoPts::Qpc,
                        "cfr" => VideoPts::Cfr,
                        other => bail!("--video-pts attend qpc ou cfr, reçu {other}"),
                    }
                }
                other => bail!("option inconnue : {other}"),
            }
        }
        Ok(args)
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse()?;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let result = run(&args);
    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn run(args: &Args) -> Result<()> {
    // ── vidéo ──
    let (device, context) = create_d3d_device()?;
    let _ = unsafe { device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true) };

    let monitor: HMONITOR =
        unsafe { MonitorFromPoint(Default::default(), MONITOR_DEFAULTTOPRIMARY) };
    let interop = windows::core::factory::<GraphicsCaptureItem, windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
    let size = item.Size()?;
    let (width, height) = (size.Width as u32, size.Height as u32);

    let dxgi: IDXGIDevice = device.cast()?;
    let winrt_device: IDirect3DDevice = unsafe {
        windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?
    }
    .cast()?;

    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = frame_pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsBorderRequired(false);

    // ── sources audio ──
    let discovered = discover_sources(args.max_sources)?;
    let mut specs: Vec<SourceSpec> = Vec::new();
    for (label, pid) in &discovered {
        specs.push(SourceSpec {
            label: label.clone(),
            pid: Some(*pid),
            stream: 0,
        });
    }
    specs.push(SourceSpec {
        label: "micro".to_string(),
        pid: None,
        stream: 0,
    });
    tracing::info!(
        "{width}×{height}@{} — {} piste(s) audio",
        args.fps,
        specs.len()
    );

    let (muxer, audio_streams) = create_muxer(
        &device,
        &args.out,
        width,
        height,
        args.fps,
        args.bitrate,
        specs.len(),
    )?;
    for (spec, stream) in specs.iter_mut().zip(&audio_streams) {
        spec.stream = *stream;
    }
    tracing::info!("MP4 multi-pistes : 1 vidéo + {} AAC acceptés par le SinkWriter",
        audio_streams.len());

    let ring = create_texture_ring(&device, width, height)?;
    let clock = MasterClock::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx): (Sender<AudioChunk>, Receiver<AudioChunk>) = channel();

    let audio_handles: Vec<_> = specs
        .iter()
        .map(|spec| {
            let (spec, clock, stop, tx) =
                (spec.clone(), clock.clone(), Arc::clone(&stop), tx.clone());
            std::thread::spawn(move || audio_thread(spec, clock, stop, tx))
        })
        .collect();
    drop(tx); // seul les threads gardent un émetteur

    session.StartCapture()?;
    unsafe { muxer.writer.BeginWriting()? };

    // Origine commune de la timeline, en 100 ns depuis le démarrage machine.
    let origin_hns = hns_since_boot(clock.origin().0, clock.frequency());
    let frame_duration = HNS_PER_SEC / args.fps as i64;
    let tick = Duration::from_nanos(1_000_000_000 / args.fps as u64);
    let total_frames = (args.minutes * 60.0 * args.fps as f64).round() as u64;

    let started = Instant::now();
    let mut next_tick = started;
    let mut has_content = false;
    let mut video_frames = 0u64;
    let mut audio_chunks = 0u64;
    // Écart entre l'horodatage QPC de la vidéo et la position CFR théorique.
    let mut video_qpc_drift_ms = 0.0f64;

    for index in 0..total_frames {
        next_tick += tick;
        let slot = (index as usize) % TEXTURE_RING;

        match frame_pool.TryGetNextFrame() {
            Ok(frame) => {
                let surface = frame.Surface()?;
                let access: windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess =
                    surface.cast()?;
                let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
                unsafe { context.CopyResource(&ring[slot], &source) };

                // Horodatage réel de la frame, sur la timeline commune. C'est
                // la différence de fond avec le Spike 1, qui comptait les
                // frames au lieu de les datter.
                let srt = frame.SystemRelativeTime()?.Duration;
                video_qpc_drift_ms =
                    ((index as i64 * frame_duration) - (srt - origin_hns)) as f64 / 10_000.0;
                has_content = true;
                drop(frame);
            }
            Err(_) if has_content => {
                let previous = (index as usize + TEXTURE_RING - 1) % TEXTURE_RING;
                unsafe { context.CopyResource(&ring[slot], &ring[previous]) };
            }
            Err(_) => {
                sleep_until(next_tick);
                continue;
            }
        }

        // PTS vidéo sur la même horloge que l'audio.
        //
        // Le premier run à 5 min datait la vidéo à l'indice de frame
        // (`index * frame_duration`) : l'écart avec le QPC atteignait −33 ms et
        // fluctuait au rythme de la gigue du `sleep`. Comme l'audio est daté au
        // QPC, cet écart était une désynchro A/V réelle dans le fichier.
        //
        // Daté au QPC, l'écart devient nul par construction — ce n'est plus une
        // valeur à mesurer mais une propriété de la conception. L'anneau de
        // segments n'y perd rien : il a besoin de keyframes régulières, pas de
        // PTS régulières, et le MP4 accepte parfaitement un débit variable.
        let video_pts = match args.video_pts {
            VideoPts::Qpc => hns_since_boot(clock.now().0, clock.frequency()) - origin_hns,
            VideoPts::Cfr => index as i64 * frame_duration,
        };
        write_video(&muxer, &ring[slot], video_pts.max(0), frame_duration)?;
        video_frames += 1;

        for chunk in rx.try_iter() {
            let pts = (chunk.boot_hns - origin_hns).max(0);
            write_audio(&muxer, &chunk, pts)?;
            audio_chunks += 1;
        }

        if index % (args.fps as u64 * 30) == 0 && index > 0 {
            tracing::info!(
                "t={:.0}s  frames={video_frames}  paquets audio={audio_chunks}  \
                 écart vidéo CFR/QPC={video_qpc_drift_ms:+.1} ms",
                started.elapsed().as_secs_f64()
            );
        }

        sleep_until(next_tick);
    }

    stop.store(true, Ordering::Relaxed);
    let reports: Vec<AudioReport> = audio_handles
        .into_iter()
        .filter_map(|h| h.join().ok())
        .collect();
    // Drainage final : les threads ont pu envoyer après la dernière itération.
    for chunk in rx.try_iter() {
        let pts = (chunk.boot_hns - origin_hns).max(0);
        write_audio(&muxer, &chunk, pts)?;
        audio_chunks += 1;
    }

    unsafe { muxer.writer.Finalize()? };
    session.Close()?;
    frame_pool.Close()?;

    report(args, &reports, started.elapsed(), video_frames, audio_chunks, video_qpc_drift_ms);
    Ok(())
}

fn report(
    args: &Args,
    reports: &[AudioReport],
    elapsed: Duration,
    video_frames: u64,
    audio_chunks: u64,
    video_drift_ms: f64,
) {
    let size = std::fs::metadata(&args.out).map(|m| m.len()).unwrap_or(0);

    tracing::info!("─── Spike 3 terminé ───");
    tracing::info!("durée          : {:.1}s", elapsed.as_secs_f64());
    tracing::info!("vidéo          : {video_frames} frames, écart CFR/QPC {video_drift_ms:+.1} ms");
    tracing::info!("audio          : {audio_chunks} paquets muxés");
    tracing::info!("fichier        : {} ({:.1} Mo)", args.out, size as f64 / 1_048_576.0);
    tracing::info!("");
    tracing::info!("piste                    échantillons      QPC     dérive");

    let mut worst = 0.0f64;
    for r in reports {
        tracing::info!(
            "{:<24} {:>8.3}s {:>8.3}s {:>+8.1} ms",
            r.label,
            r.sample_seconds(),
            r.qpc_seconds(),
            r.drift_ms()
        );
        worst = worst.max(r.drift_ms().abs());
    }

    // Désynchronisation relative entre pistes : c'est ce qu'un auditeur perçoit.
    if reports.len() > 1 {
        let drifts: Vec<f64> = reports.iter().map(|r| r.drift_ms()).collect();
        let spread = drifts.iter().cloned().fold(f64::MIN, f64::max)
            - drifts.iter().cloned().fold(f64::MAX, f64::min);
        tracing::info!("écart entre pistes : {spread:.1} ms");
    }

    tracing::info!("");
    if worst.max(video_drift_ms.abs()) < 40.0 {
        tracing::info!("✅ CRITÈRE TENU : dérive maximale {:.1} ms < 40 ms", worst.max(video_drift_ms.abs()));
    } else {
        tracing::error!("❌ CRITÈRE MANQUÉ : dérive maximale {:.1} ms ≥ 40 ms", worst.max(video_drift_ms.abs()));
    }
}

fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        std::thread::sleep(deadline - now);
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.context("device nul")?,
        context.context("contexte nul")?,
    ))
}

fn create_texture_ring(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    (0..TEXTURE_RING)
        .map(|_| {
            let mut texture = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
            texture.context("CreateTexture2D null")
        })
        .collect()
}
